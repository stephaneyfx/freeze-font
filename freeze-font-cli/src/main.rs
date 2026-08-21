use ab_glyph::Font as _;
use anyhow::Context;
use base64::Engine;
use clap::{Args, Parser, Subcommand};
use freeze_font::{Platform, feature_substitutions, feature_ui_label, substitutions};
use image::{ImageFormat, RgbaImage};
use itertools::Itertools;
use read_fonts::{TableProvider, types::Tag};
use std::{
    collections::{HashMap, HashSet},
    io::BufWriter,
    path::PathBuf,
    str::FromStr,
};
use url::Url;

const HTML_STYLE: &str = r##"
table {
    border-collapse: collapse;
}

td, th {
    border: 1px solid #dddddd;
    padding: 8px;
}
"##;

#[derive(Debug, Parser)]
struct Cli {
    #[command(flatten)]
    font: FontSelector,
    #[command(subcommand)]
    cmd: CliCommand,
}

impl Cli {
    fn run(self) -> anyhow::Result<()> {
        let (font_bytes, font_index) = if let Some(path) = self.font.path {
            (std::fs::read(&path).context("failed to read font file")?, 0)
        } else {
            let font_name = self.font.name.unwrap();
            let mut font_db = fontdb::Database::new();
            font_db.load_system_fonts();
            let font_id = font_db
                .query(&fontdb::Query {
                    families: &[fontdb::Family::Name(&font_name)],
                    ..Default::default()
                })
                .context("failed to find font")?;
            if let Some((source, index)) = font_db.face_source(font_id) {
                match source {
                    fontdb::Source::SharedFile(path, ..) | fontdb::Source::File(path) => {
                        println!("Using font at index {index} in {}", path.display())
                    }
                    fontdb::Source::Binary(_) => {}
                }
            }
            font_db
                .with_face_data(font_id, |bytes, index| (bytes.to_vec(), index))
                .context("failed to load font data")?
        };
        self.cmd.run(font_bytes, font_index)
    }
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    Draw(DrawCommand),
    #[command(name = "alts")]
    Alternates(Alternates),
    Features(Features),
    Names(Names),
    #[command(name = "fvar")]
    Variations(Variations),
    Freeze(Freeze),
}

impl CliCommand {
    fn run(self, font_bytes: Vec<u8>, font_index: u32) -> anyhow::Result<()> {
        match self {
            CliCommand::Draw(cmd) => cmd.run(font_bytes, font_index),
            CliCommand::Alternates(cmd) => cmd.run(font_bytes, font_index),
            CliCommand::Features(cmd) => cmd.run(font_bytes, font_index),
            CliCommand::Names(cmd) => cmd.run(font_bytes, font_index),
            CliCommand::Variations(cmd) => cmd.run(font_bytes, font_index),
            CliCommand::Freeze(cmd) => cmd.run(font_bytes, font_index),
        }
    }
}

#[derive(Args, Debug)]
struct DrawCommand {
    #[command(flatten)]
    glyph: GlyphSelector,
    #[arg(long)]
    scale: f32,
}

impl DrawCommand {
    fn run(self, font_bytes: Vec<u8>, font_index: u32) -> anyhow::Result<()> {
        let font = ab_glyph::FontVec::try_from_vec_and_index(font_bytes, font_index)
            .context("failed to parse font file")?;
        let glyph_id = self.glyph.c.map_or_else(
            || ab_glyph::GlyphId(self.glyph.id.unwrap()),
            |c| font.glyph_id(c),
        );
        let glyph = glyph_id.with_scale_and_position(self.scale, (0.0_f32, 0.0_f32));
        let outline = font
            .outline_glyph(glyph)
            .with_context(|| format!("outline not found for glyph ID {}", glyph_id.0))?;
        let bounds = outline.px_bounds();
        let width = bounds.width() as usize;
        let height = bounds.height() as usize;
        let mut canvas = vec![b' '; width * height];
        outline.draw(|x, y, v| {
            let (x, y) = (x as usize, y as usize);
            if x >= width || y >= height {
                return;
            }
            canvas[x + y * width] = coverage(v);
        });
        canvas
            .chunks(width)
            .for_each(|line| println!("{}", std::str::from_utf8(line).unwrap()));
        Ok(())
    }
}

#[derive(Args, Debug)]
struct Alternates {
    #[command(flatten)]
    glyph: GlyphSelector,
}

impl Alternates {
    fn run(self, font_bytes: Vec<u8>, font_index: u32) -> anyhow::Result<()> {
        let font = read_fonts::FontRef::from_index(&font_bytes, font_index)?;
        let cmap = font.cmap()?;
        let mut cmap_formats = cmap
            .encoding_records()
            .iter()
            .map(|r| r.subtable(cmap.offset_data()).map(|t| t.format()))
            .collect::<Result<HashSet<_>, _>>()?
            .into_iter()
            .collect::<Vec<_>>();
        cmap_formats.sort();
        println!("cmap formats: {cmap_formats:?}");
        let target = if let Some(c) = self.glyph.c {
            font.cmap()?
                .map_codepoint(c)
                .context("failed to get glyph for character")?
                .to_u32()
                .try_into()
                .context("glyph index too large")?
        } else {
            self.glyph.id.unwrap()
        };
        let gsub = font.gsub()?;
        let subs = gsub.lookup_list()?.lookups().iter();
        let mut alternates = HashSet::<u16>::new();
        for lookup in subs {
            for sub in substitutions(lookup?) {
                let (src, dst) = sub?;
                if src == target {
                    alternates.insert(dst);
                }
            }
        }
        let mut alternates = alternates.into_iter().collect::<Vec<_>>();
        alternates.sort();
        println!("{target}: {alternates:?}");
        println!();
        for r in cmap.encoding_records() {
            if let Ok(read_fonts::tables::cmap::CmapSubtable::Format4(cmap4)) =
                r.subtable(cmap.offset_data())
            {
                println!(
                    "cmap4 start: [{}], end: [{}]",
                    cmap4.start_code()[cmap4.start_code().len().saturating_sub(2)..]
                        .iter()
                        .format(","),
                    cmap4.end_code()[cmap4.end_code().len().saturating_sub(2)..]
                        .iter()
                        .format(","),
                );
            }
        }
        Ok(())
    }
}

#[derive(Args, Debug)]
struct Features {
    #[arg(long)]
    scale: f32,
    #[arg(long, short)]
    out: PathBuf,
}

impl Features {
    fn run(self, font_bytes: Vec<u8>, font_index: u32) -> anyhow::Result<()> {
        let font = read_fonts::FontRef::from_index(&font_bytes, font_index)?;
        let name = font.name()?;
        let gsub = font.gsub()?;
        let sub_lookup_list = gsub.lookup_list()?;
        let feature_list = gsub.feature_list()?;
        let mut features = HashMap::<Tag, FeatureInfo>::new();
        for feature_record in feature_list.feature_records() {
            let feature = feature_record.feature(feature_list.offset_data())?;
            let info = features
                .entry(feature_record.feature_tag())
                .or_insert_with(|| FeatureInfo::new(feature_record.feature_tag()));
            if info.label.is_none()
                && let Some(label) = feature
                    .feature_params()
                    .transpose()?
                    .and_then(|p| feature_ui_label(&p, &name).transpose())
                    .transpose()?
            {
                info.label = Some(label);
            }
            for sub in feature_substitutions(feature, &sub_lookup_list) {
                let sub = sub?;
                info.substitutions.push(sub);
            }
        }
        features
            .values_mut()
            .for_each(|info| info.substitutions.sort_by_key(|&(k, _)| k));
        let feature_ui_labels = features
            .values()
            .filter_map(|f| f.label.as_ref().map(|label| (f.tag, label.clone())))
            .collect::<HashMap<_, _>>();
        let features = features
            .into_values()
            .sorted_by_key(|f| f.tag)
            .collect::<Vec<_>>();
        let ab_font = ab_glyph::FontRef::try_from_slice_and_index(&font_bytes, font_index)?;
        let doc = b"<!DOCTYPE html>\n".to_vec();
        let mut out = xml::EmitterConfig::new()
            .write_document_declaration(false)
            .perform_indent(true)
            .create_writer(doc);
        nestxml::html::html(&mut out).write(|out| {
            nestxml::html::head(out).write(|out| {
                nestxml::html::title(out).text("Font features")?;
                nestxml::html::style(out).text(HTML_STYLE)
            })?;
            nestxml::html::body(out).write(|out| {
                for feature in features {
                    match feature.label {
                        Some(label) => {
                            nestxml::html::h1(out).text(&format!("{} ({label})", feature.tag))
                        }
                        None => nestxml::html::h1(out).text(&feature.tag.to_string()),
                    }?;
                    if feature.substitutions.is_empty() {
                        nestxml::html::p(out).text("No substitutions found")?;
                        continue;
                    }
                    nestxml::html::table(out).write(|out| {
                        nestxml::html::tr(out).write(|out| {
                            nestxml::html::th(out).text("Original")?;
                            nestxml::html::th(out).text("Substitution")?;
                            nestxml::html::th(out).text("Before")?;
                            nestxml::html::th(out).text("After")
                        })?;
                        for (src, dst) in feature.substitutions {
                            nestxml::html::tr(out).write(|out| {
                                nestxml::html::td(out).text(&src.to_string())?;
                                nestxml::html::td(out).text(&dst.to_string())?;
                                match glyph_to_image(&ab_font, src, self.scale) {
                                    Some(img) => nestxml::html::td(out).write(|out| {
                                        nestxml::html::img(out)
                                            .attr("src", img_base64_uri(&img))
                                            .empty()
                                    }),
                                    None => nestxml::html::td(out).empty(),
                                }?;
                                match glyph_to_image(&ab_font, dst, self.scale) {
                                    Some(img) => nestxml::html::td(out).write(|out| {
                                        nestxml::html::img(out)
                                            .attr("src", img_base64_uri(&img))
                                            .empty()
                                    }),
                                    None => nestxml::html::td(out).empty(),
                                }
                            })?;
                        }
                        Ok(())
                    })?;
                }
                Ok(())
            })
        })?;
        std::fs::write(&self.out, out.into_inner())?;
        let features = gsub
            .feature_list()?
            .feature_records()
            .iter()
            .unique_by(|f| f.feature_tag)
            .map(|f| f.feature_tag.get())
            .sorted()
            .format_with(", ", |tag, f| match feature_ui_labels.get(&tag) {
                Some(label) => f(&format_args!("{tag} ({label})")),
                None => f(&tag),
            });
        println!("features: {features}");
        Ok(())
    }
}

#[derive(Args, Debug)]
struct Names;

impl Names {
    fn run(self, font_bytes: Vec<u8>, font_index: u32) -> anyhow::Result<()> {
        let font = read_fonts::FontRef::from_index(&font_bytes, font_index)?;
        let name_table = font.name()?;
        for r in name_table.name_record() {
            println!(
                "[platform={}, encoding={}, language={}, name_id={}] {}",
                r.platform_id(),
                r.encoding_id(),
                r.language_id(),
                r.name_id(),
                r.string(name_table.string_data())?,
            )
        }
        Ok(())
    }
}

#[derive(Args, Debug)]
struct Variations;

impl Variations {
    fn run(self, font_bytes: Vec<u8>, font_index: u32) -> anyhow::Result<()> {
        let font = read_fonts::FontRef::from_index(&font_bytes, font_index)?;
        let fvar = font.fvar()?;
        let name = font.name()?;
        println!("variation axes:");
        for axis in fvar.axes()? {
            println!("  tag={}", axis.axis_tag());
            println!("  min={}", axis.min_value());
            println!("  default={}", axis.default_value());
            println!("  max={}", axis.max_value());
            println!("  flags={}", axis.flags());
            println!(
                "  name={}",
                freeze_font::get_name(&name, axis.axis_name_id())?.context("name ID not found")?
            );
            println!()
        }
        println!("instances:");
        for instance in fvar.instances()?.iter() {
            let instance = instance?;
            println!(
                "  subfamily={}",
                freeze_font::get_name(&name, instance.subfamily_name_id)?
                    .context("name ID not found")?
            );
            println!("  flags={}", instance.flags);
            println!(
                "  coordinates={}",
                instance.coordinates.iter().map(|c| c.get()).format(", ")
            );
            println!(
                "  postscript name={:?}",
                instance
                    .post_script_name_id
                    .map(|id| freeze_font::get_name(&name, id)?.context("name ID not found"))
                    .transpose()?
            );
            println!();
        }
        Ok(())
    }
}

#[derive(Args, Debug)]
struct Freeze {
    #[arg(long = "feature")]
    features: Vec<String>,
    #[arg(long = "sub")]
    substitutions: Vec<Substitution>,
    #[arg(long = "exclude-platform")]
    excluded_cmap_platforms: Vec<Platform>,
    #[arg(long)]
    clean_names: bool,
    #[arg(long)]
    copyright: Option<String>,
    #[arg(long)]
    family: Option<String>,
    #[arg(long)]
    subfamily: Option<String>,
    #[arg(long)]
    uid: Option<String>,
    #[arg(long)]
    full_name: Option<String>,
    #[arg(long, value_parser = parse_version)]
    version: Option<(u16, u16)>,
    #[arg(long)]
    postscript_name: Option<String>,
    #[arg(long)]
    description: Option<String>,
    #[arg(long)]
    vendor_url: Option<Url>,
    #[arg(short, long = "out")]
    output: PathBuf,
}

impl Freeze {
    fn run(self, font_bytes: Vec<u8>, font_index: u32) -> anyhow::Result<()> {
        let mut builder = freeze_font::Freeze::new()
            .add_features(self.features)
            .add_substitutions(
                self.substitutions
                    .into_iter()
                    .map(|sub| (sub.original_glyph_id, sub.sub_glyph_id)),
            )
            .exclude_cmap_platforms(self.excluded_cmap_platforms)
            .clean_names(self.clean_names);
        if let Some(s) = self.copyright {
            builder = builder.with_copyright(s);
        }
        if let Some(s) = self.family {
            builder = builder.with_family(s);
        }
        if let Some(s) = self.subfamily {
            builder = builder.with_subfamily(s);
        }
        if let Some(s) = self.uid {
            builder = builder.with_unique_id(s);
        }
        if let Some(s) = self.full_name {
            builder = builder.with_full_name(s);
        }
        if let Some((major, minor)) = self.version {
            builder = builder.with_version(major, minor);
        }
        if let Some(s) = self.postscript_name {
            builder = builder.with_postscript_name(s);
        }
        if let Some(s) = self.description {
            builder = builder.with_description(s);
        }
        if let Some(s) = self.vendor_url {
            builder = builder.with_vendor_url(s);
        }
        let new_font = builder.build(&font_bytes, font_index)?;
        std::fs::write(&self.output, new_font).context("failed to write font file")?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct Substitution {
    original_glyph_id: u16,
    sub_glyph_id: u16,
}

impl FromStr for Substitution {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (before, after) = s
            .split_once(':')
            .context("failed to find ':' in substitution")?;
        Ok(Self {
            original_glyph_id: before.parse()?,
            sub_glyph_id: after.parse()?,
        })
    }
}

#[derive(Args, Debug)]
#[group(required = true, multiple = false)]
struct GlyphSelector {
    #[arg(long = "char")]
    c: Option<char>,
    #[arg(long)]
    id: Option<u16>,
}

#[derive(Args, Debug)]
#[group(required = true, multiple = false)]
struct FontSelector {
    #[arg(long, global = true)]
    path: Option<PathBuf>,
    #[arg(long, global = true)]
    name: Option<String>,
}

fn coverage(v: f32) -> u8 {
    const MAP: &[u8] = b" .-:=+x#@";
    MAP[((v * (MAP.len() - 1) as f32) as usize).clamp(0, MAP.len() - 1)]
}

fn parse_version(s: &str) -> anyhow::Result<(u16, u16)> {
    let (major, minor) = s
        .split_once('.')
        .context("version must contain a single dot")?;
    let major = major
        .parse()
        .context("major version number must be an u16")?;
    let minor = minor
        .parse()
        .context("minor version number must be an u16")?;
    Ok((major, minor))
}

fn glyph_to_image(font: &ab_glyph::FontRef<'_>, glyph_id: u16, scale: f32) -> Option<RgbaImage> {
    let glyph = ab_glyph::GlyphId(glyph_id).with_scale_and_position(scale, (0.0_f32, 0.0_f32));
    let outline = font.outline_glyph(glyph)?;
    let bounds = outline.px_bounds();
    let width = bounds.width() as u32;
    let height = bounds.height() as u32;
    let mut canvas = RgbaImage::new(width, height);
    outline.draw(|x, y, v| {
        if x >= width || y >= height {
            return;
        }
        canvas[(x, y)].0[3] = canvas[(x, y)].0[3].saturating_add((v * 255.0) as u8);
    });
    Some(canvas)
}

fn img_base64_uri(img: &RgbaImage) -> String {
    let mut buf = BufWriter::new(std::io::Cursor::new(Vec::new()));
    img.write_to(&mut buf, ImageFormat::Png)
        .expect("writing png to memory does not fail");
    format!(
        "data:image/png;base64,{}",
        base64::prelude::BASE64_STANDARD.encode(
            buf.into_inner()
                .expect("writing to memory does not fail")
                .into_inner()
        )
    )
}

#[derive(Clone, Debug)]
struct FeatureInfo {
    tag: Tag,
    label: Option<String>,
    substitutions: Vec<(u16, u16)>,
}

impl FeatureInfo {
    fn new(tag: Tag) -> Self {
        Self {
            tag,
            label: None,
            substitutions: Vec::new(),
        }
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    cli.run()
}
