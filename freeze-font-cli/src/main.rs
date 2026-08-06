use ab_glyph::Font as _;
use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use freeze_font::{Platform, cmap_rev_mapping, feature_substitutions, substitutions};
use itertools::Itertools;
use read_fonts::{TableProvider, types::Tag};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    str::FromStr,
};
use url::Url;

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
    Freeze(Freeze),
}

impl CliCommand {
    fn run(self, font_bytes: Vec<u8>, font_index: u32) -> anyhow::Result<()> {
        match self {
            CliCommand::Draw(cmd) => cmd.run(font_bytes, font_index),
            CliCommand::Alternates(cmd) => cmd.run(font_bytes, font_index),
            CliCommand::Features(cmd) => cmd.run(font_bytes, font_index),
            CliCommand::Names(cmd) => cmd.run(font_bytes, font_index),
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
        let outline = font.outline_glyph(glyph).unwrap();
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
            if let Ok(t) = r.subtable(cmap.offset_data())
                && let Some(gid) = t.map_codepoint(b'I')
            {
                println!(
                    "{gid} found in table cmap{}, platform {:?}, encoding {}",
                    t.format(),
                    r.platform_id(),
                    r.encoding_id()
                );
            }
        }
        Ok(())
    }
}

#[derive(Args, Debug)]
struct Features {
    #[arg(long = "exclude-platform")]
    excluded_cmap_platforms: Vec<Platform>,
}

impl Features {
    fn run(self, font_bytes: Vec<u8>, font_index: u32) -> anyhow::Result<()> {
        let excluded_platforms = self
            .excluded_cmap_platforms
            .into_iter()
            .map(Platform::id)
            .collect::<HashSet<_>>();
        let font = read_fonts::FontRef::from_index(&font_bytes, font_index)?;
        let cmap = font.cmap()?;
        let mapping = cmap_rev_mapping(&cmap)?;
        let gsub = font.gsub()?;
        let sub_lookup_list = gsub.lookup_list()?;
        let feature_list = gsub.feature_list()?;
        let mut subs_by_tag = HashMap::<Tag, HashSet<(char, u16)>>::new();
        for feature_record in feature_list.feature_records() {
            let feature = feature_record.feature(feature_list.offset_data())?;
            let subs = subs_by_tag.entry(feature_record.feature_tag()).or_default();
            for sub in feature_substitutions(feature, &sub_lookup_list) {
                let sub = sub?;
                subs.extend(
                    mapping
                        .get(&sub.0)
                        .into_iter()
                        .flatten()
                        .filter(|(platform, _)| !excluded_platforms.contains(platform))
                        .flat_map(|(_, codepoints)| codepoints)
                        .copied()
                        .map(|c| (c, sub.1)),
                );
            }
        }
        for (tag, subs) in subs_by_tag.iter().sorted_by_key(|&(&tag, _)| tag) {
            println!("{tag}:");
            for &(c, sub) in subs.iter().sorted_by_key(|&&(c, _)| c) {
                println!("  {c} [{}]: {sub}", u32::from(c));
            }
        }
        println!();
        let features = gsub
            .feature_list()?
            .feature_records()
            .iter()
            .unique_by(|f| f.feature_tag)
            .map(|f| f.feature_tag)
            .sorted()
            .join(", ");
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
                    .map(|sub| (sub.character, sub.glyph)),
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
    character: char,
    glyph: u16,
}

impl FromStr for Substitution {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (codepoint, glyph) = s
            .split_once(':')
            .context("failed to find ':' in substitution")?;
        Ok(Self {
            character: {
                let mut chars = codepoint.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => c,
                    _ => match codepoint.strip_prefix("0x") {
                        Some(digits) => u32::from_str_radix(digits, 16)?.try_into()?,
                        _ => u32::from_str_radix(codepoint, 10)?.try_into()?,
                    },
                }
            },
            glyph: glyph.parse()?,
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

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    cli.run()
}
