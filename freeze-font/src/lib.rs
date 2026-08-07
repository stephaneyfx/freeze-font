use either::Either::{self, Left, Right};
use itertools::Itertools;
use read_fonts::{
    ReadError, TableProvider,
    tables::{
        cmap::Cmap,
        gsub::{SingleSubst, SubstitutionLookup, SubstitutionLookupList},
        layout::Feature,
    },
    types::{GlyphId, NameId},
};
use std::{
    collections::{HashMap, HashSet},
    fmt::{self, Display},
    str::FromStr,
};
use thiserror::Error;
use url::Url;
use write_fonts::{FontBuilder, OffsetMarker, from_obj::ToOwnedTable, tables::cmap::CmapConflict};

const DEFAULT_LANG_ID: u16 = 1033;
const DEFAULT_ENCODING_ID: u16 = 1;
const DEFAULT_PLATFORM: Platform = Platform::Windows;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Read(#[from] read_fonts::ReadError),
    #[error("glyph ID out of range")]
    GlyphIdOutOfRange,
    #[error("invalid codepoint {0}")]
    InvalidCodepoint(u32),
    #[error(transparent)]
    Builder(#[from] write_fonts::BuilderError),
    #[error(transparent)]
    CmapConflict(#[from] CmapConflict),
    #[error("name record not found for ID {0}")]
    NameRecordNotFound(u16),
}

pub fn feature_substitutions<'a>(
    feature: Feature<'a>,
    sub_lookup_list: &SubstitutionLookupList<'a>,
) -> impl Iterator<Item = Result<(u16, u16), Error>> {
    feature.lookup_list_indices().iter().flat_map(move |index| {
        let index = usize::from(index.get());
        match sub_lookup_list.lookups().get(index) {
            Ok(sub) => Right(substitutions(sub)),
            Err(e) => Left(std::iter::once(Err(e.into()))),
        }
    })
}

pub fn substitutions(
    lookup: SubstitutionLookup<'_>,
) -> impl Iterator<Item = Result<(u16, u16), Error>> {
    match lookup {
        SubstitutionLookup::Single(lookup) => Left(
            lookup
                .subtables()
                .iter()
                .map(|sub| match sub? {
                    SingleSubst::Format1(sub) => Ok(Left(sub.coverage()?.iter().map(move |g| {
                        let g = g.to_u16();
                        Ok((
                            g,
                            u16::try_from(i32::from(g) + i32::from(sub.delta_glyph_id()))
                                .map_err(|_| Error::GlyphIdOutOfRange)?,
                        ))
                    }))),
                    SingleSubst::Format2(sub) => Ok(Right(
                        sub.coverage()?
                            .iter()
                            .map(|g| g.to_u16())
                            .zip(sub.substitute_glyph_ids().iter().map(|g| g.get().to_u16()))
                            .map(Ok),
                    )),
                })
                .flat_map(|r| Either::from(r).map_left(|e| std::iter::once(Err(e)))),
        ),
        SubstitutionLookup::Alternate(lookup) => Right(Left(
            lookup
                .subtables()
                .iter()
                .map(|sub| {
                    let sub = sub?;
                    Ok(sub
                        .coverage()?
                        .iter()
                        .map(|g| g.to_u16())
                        .zip(sub.alternate_sets().iter().map(|set| {
                            set.map(|set| set.alternate_glyph_ids()).unwrap_or_default()
                        }))
                        .flat_map(|(src, dst)| {
                            dst.iter().map(move |g| Ok((src, g.get().to_u16())))
                        }))
                })
                .flat_map(|r| Either::from(r).map_left(|e| std::iter::once(Err(e)))),
        )),
        _ => Right(Right(std::iter::empty())),
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Freeze {
    features: Vec<String>,
    substitutions: Vec<(char, u16)>,
    cmap_excluded_platforms: HashSet<Platform>,
    clean_names: bool,
    copyright: Option<String>,
    family: Option<String>,
    subfamily: Option<String>,
    unique_id: Option<String>,
    full_name: Option<String>,
    version: Option<(u16, u16)>,
    postscript_name: Option<String>,
    description: Option<String>,
    vendor_url: Option<Url>,
}

impl Freeze {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn add_feature<S>(mut self, feature: S) -> Self
    where
        S: Into<String>,
    {
        self.features.push(feature.into());
        self
    }

    pub fn add_features<I>(mut self, features: I) -> Self
    where
        I: IntoIterator<Item: Into<String>>,
    {
        self.features.extend(features.into_iter().map(Into::into));
        self
    }

    pub fn add_substitution(mut self, character: char, glyph_id: u16) -> Self {
        self.substitutions.push((character, glyph_id));
        self
    }

    pub fn add_substitutions<I>(mut self, substitutions: I) -> Self
    where
        I: IntoIterator<Item = (char, u16)>,
    {
        self.substitutions.extend(substitutions);
        self
    }

    pub fn exclude_cmap_platform(mut self, p: Platform) -> Self {
        self.cmap_excluded_platforms.insert(p);
        self
    }

    pub fn exclude_cmap_platforms<I>(mut self, platforms: I) -> Self
    where
        I: IntoIterator<Item = Platform>,
    {
        self.cmap_excluded_platforms.extend(platforms);
        self
    }

    pub fn clean_names(self, yes: bool) -> Self {
        Self {
            clean_names: yes,
            ..self
        }
    }

    pub fn with_copyright<S>(self, right: S) -> Self
    where
        S: Into<String>,
    {
        Self {
            copyright: Some(right.into()),
            ..self
        }
    }

    pub fn with_family<S>(self, family: S) -> Self
    where
        S: Into<String>,
    {
        Self {
            family: Some(family.into()),
            ..self
        }
    }

    pub fn with_subfamily<S>(self, subfamily: S) -> Self
    where
        S: Into<String>,
    {
        Self {
            subfamily: Some(subfamily.into()),
            ..self
        }
    }

    pub fn with_unique_id<S>(self, id: S) -> Self
    where
        S: Into<String>,
    {
        Self {
            unique_id: Some(id.into()),
            ..self
        }
    }

    pub fn with_full_name<S>(self, name: S) -> Self
    where
        S: Into<String>,
    {
        Self {
            full_name: Some(name.into()),
            ..self
        }
    }

    pub fn with_version(self, major: u16, minor: u16) -> Self {
        Self {
            version: Some((major, minor)),
            ..self
        }
    }

    pub fn with_postscript_name<S>(self, name: S) -> Self
    where
        S: Into<String>,
    {
        Self {
            postscript_name: Some(name.into()),
            ..self
        }
    }

    pub fn with_description<S>(self, desc: S) -> Self
    where
        S: Into<String>,
    {
        Self {
            description: Some(desc.into()),
            ..self
        }
    }

    pub fn with_vendor_url(self, url: Url) -> Self {
        Self {
            vendor_url: Some(url),
            ..self
        }
    }

    pub fn build(&self, font_bytes: &[u8], font_index: u32) -> Result<Vec<u8>, Error> {
        let cmap_excluded_platforms = self
            .cmap_excluded_platforms
            .iter()
            .copied()
            .map(Platform::id)
            .collect::<HashSet<_>>();
        let font = read_fonts::FontRef::from_index(font_bytes, font_index)?;
        let original_cmap = font.cmap()?;
        let original_gsub = font.gsub()?;
        let feature_list = original_gsub.feature_list()?;
        let sub_lookup_list = original_gsub.lookup_list()?;
        let subs_from_features = self
            .features
            .iter()
            .flat_map(|tag| {
                feature_list
                    .feature_records()
                    .iter()
                    .filter(|f| f.feature_tag() == tag.as_str())
                    .map(|f| f.feature(feature_list.offset_data()))
            })
            .flat_map(|feature| match feature {
                Ok(feature) => Right(
                    feature_substitutions(feature, &sub_lookup_list)
                        .map_ok(|(src, dst)| (GlyphId::new(src.into()), GlyphId::new(dst.into()))),
                ),
                Err(e) => Left(std::iter::once(Err(e.into()))),
            })
            .collect::<Result<HashSet<_>, _>>()?;
        let explicit_subs = self
            .substitutions
            .iter()
            .copied()
            .filter_map(|(c, glyph_id)| {
                original_cmap
                    .map_codepoint(c)
                    .map(|original_glyph| (original_glyph, GlyphId::new(glyph_id.into())))
            });
        let all_subs = subs_from_features
            .into_iter()
            .chain(explicit_subs)
            .collect::<HashMap<_, _>>();
        let mapping = original_cmap
            .encoding_records()
            .iter()
            .filter(|r| !cmap_excluded_platforms.contains(&r.platform_id().into()))
            .flat_map(|r| match r.subtable(original_cmap.offset_data()) {
                Ok(t) => Right(t.iter().map(|(codepoint, glyph)| {
                    char::try_from(codepoint)
                        .map_err(|_| Error::InvalidCodepoint(codepoint))
                        .map(|c| (c, all_subs.get(&glyph).copied().unwrap_or(glyph)))
                })),
                Err(e) => Left(std::iter::once(Err(e.into()))),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let cmap = write_fonts::tables::cmap::Cmap::from_mappings(mapping)?;
        let original_name = font.name()?;
        let mut name: write_fonts::tables::name::Name = original_name.to_owned_table();
        if self.clean_names {
            name.name_record.retain(name_record_is_default);
        }
        if let Some(s) = &self.copyright {
            set_name(&mut name.name_record, NameId::COPYRIGHT_NOTICE, s);
        }
        if let Some(s) = &self.family {
            set_name(&mut name.name_record, NameId::FAMILY_NAME, s);
            let mut s = s.to_owned();
            s.retain(|c| !c.is_whitespace());
            set_name(
                &mut name.name_record,
                NameId::VARIATIONS_POSTSCRIPT_NAME_PREFIX,
                s,
            );
        }
        if let Some(s) = &self.subfamily {
            set_name(&mut name.name_record, NameId::SUBFAMILY_NAME, s);
        }
        if let Some(s) = &self.unique_id {
            set_name(&mut name.name_record, NameId::UNIQUE_ID, s);
        }
        if let Some(s) = &self.full_name {
            set_name(&mut name.name_record, NameId::FULL_NAME, s);
        }
        if let Some((major, minor)) = self.version {
            set_name(
                &mut name.name_record,
                NameId::VERSION_STRING,
                format!("Version {major}.{minor}"),
            );
        }
        if let Some(s) = &self.postscript_name {
            set_name(&mut name.name_record, NameId::POSTSCRIPT_NAME, s);
        }
        if let Some(s) = &self.description {
            set_name(&mut name.name_record, NameId::DESCRIPTION, s);
        }
        if let Some(s) = &self.vendor_url {
            set_name(&mut name.name_record, NameId::VENDOR_URL, s.to_string());
        }
        let fvar = self
            .family
            .as_ref()
            .map(|family| build_fvar(&font, family, &mut name, &original_name))
            .transpose()?
            .flatten();
        name.name_record.sort();
        let mut builder = FontBuilder::new();
        builder.add_table(&name)?.add_table(&cmap)?;
        if let Some(fvar) = fvar {
            builder.add_table(&fvar)?;
        }
        Ok(builder.copy_missing_tables(font).build())
    }
}

pub trait NameRecord {
    fn platform_id(&self) -> PlatformId;
    fn encoding_id(&self) -> u16;
    fn language_id(&self) -> u16;
}

impl NameRecord for read_fonts::tables::name::NameRecord {
    fn platform_id(&self) -> PlatformId {
        PlatformId(self.platform_id.get())
    }

    fn encoding_id(&self) -> u16 {
        self.encoding_id.get()
    }

    fn language_id(&self) -> u16 {
        self.language_id.get()
    }
}

impl NameRecord for write_fonts::tables::name::NameRecord {
    fn platform_id(&self) -> PlatformId {
        PlatformId(self.platform_id)
    }

    fn encoding_id(&self) -> u16 {
        self.encoding_id
    }

    fn language_id(&self) -> u16 {
        self.language_id
    }
}

fn name_record_is_default<R>(r: &R) -> bool
where
    R: NameRecord,
{
    r.platform_id() == DEFAULT_PLATFORM.id()
        && r.encoding_id() == DEFAULT_ENCODING_ID
        && r.language_id() == DEFAULT_LANG_ID
}

fn set_name<S>(names: &mut Vec<write_fonts::tables::name::NameRecord>, id: NameId, value: S)
where
    S: Into<String>,
{
    match names
        .iter_mut()
        .find(|r| name_record_is_default(*r) && r.name_id == id)
    {
        Some(r) => r.string.set(value),
        None => names.push(write_fonts::tables::name::NameRecord::new(
            DEFAULT_PLATFORM.id().0,
            DEFAULT_ENCODING_ID,
            DEFAULT_LANG_ID,
            id,
            OffsetMarker::new(value.into()),
        )),
    }
}

pub fn get_name(
    name: &read_fonts::tables::name::Name<'_>,
    id: NameId,
) -> Result<Option<String>, Error> {
    name.name_record()
        .iter()
        .find(|&r| name_record_is_default(r) && r.name_id.get() == id)
        .map(|r| r.string(name.string_data()).map(|s| s.to_string()))
        .transpose()
        .map_err(Into::into)
}

pub fn cmap_rev_mapping(
    cmap: &Cmap,
) -> Result<HashMap<u16, HashMap<PlatformId, HashSet<char>>>, Error> {
    cmap.encoding_records().iter().try_fold(
        HashMap::<u16, HashMap<PlatformId, HashSet<char>>>::new(),
        |acc, r| {
            r.subtable(cmap.offset_data())?.iter().try_fold(
                acc,
                |mut acc, (codepoint, glyph_id)| {
                    let c = char::try_from(codepoint)
                        .map_err(|_| Error::InvalidCodepoint(codepoint))?;
                    let id =
                        u16::try_from(glyph_id.to_u32()).map_err(|_| Error::GlyphIdOutOfRange)?;
                    acc.entry(id)
                        .or_default()
                        .entry(r.platform_id().into())
                        .or_default()
                        .insert(c);
                    Ok(acc)
                },
            )
        },
    )
}

fn build_fvar(
    font: &read_fonts::FontRef<'_>,
    family: &str,
    name: &mut write_fonts::tables::name::Name,
    original_name: &read_fonts::tables::name::Name,
) -> Result<Option<write_fonts::tables::fvar::Fvar>, Error> {
    let original_fvar = match font.fvar() {
        Ok(t) => t,
        Err(ReadError::TableIsMissing(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut family = family.to_owned();
    family.retain(|c| !c.is_whitespace());
    let mut fvar: write_fonts::tables::fvar::Fvar = original_fvar.to_owned_table();
    let mut name_appender = NameAppender::new(name);
    for instance in &mut fvar.axis_instance_arrays.instances {
        let name_id = instance
            .post_script_name_id
            .unwrap_or_else(|| name_appender.next_id());
        let subfamily = get_name(original_name, instance.subfamily_name_id)?
            .ok_or_else(|| Error::NameRecordNotFound(instance.subfamily_name_id.to_u16()))?;
        let postscript_name = format!("{family}-{subfamily}");
        set_name(
            &mut name_appender.name.name_record,
            name_id,
            postscript_name,
        );
    }
    Ok(Some(fvar))
}

#[derive(Debug)]
struct NameAppender<'a> {
    name: &'a mut write_fonts::tables::name::Name,
    next_id: u16,
}

impl<'a> NameAppender<'a> {
    fn new(name: &'a mut write_fonts::tables::name::Name) -> Self {
        let next_id = name
            .name_record
            .iter()
            .filter(|r| name_record_is_default(*r))
            .map(|r| r.name_id.to_u16())
            .max()
            .unwrap_or(255)
            + 1;
        Self { name, next_id }
    }

    fn next_id(&mut self) -> NameId {
        let next = self.next_id;
        self.next_id += 1;
        NameId::new(next)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PlatformId(pub u16);

impl From<Platform> for PlatformId {
    fn from(value: Platform) -> Self {
        value.id()
    }
}

impl From<read_fonts::tables::cmap::PlatformId> for PlatformId {
    fn from(value: read_fonts::tables::cmap::PlatformId) -> Self {
        Self(value as u16)
    }
}

impl From<PlatformId> for read_fonts::tables::cmap::PlatformId {
    fn from(value: PlatformId) -> Self {
        Self::new(value.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, enum_iterator::Sequence)]
#[repr(u16)]
pub enum Platform {
    Unicode,
    Macintosh,
    Iso,
    Windows,
    Custom,
}

impl Platform {
    pub const fn id(self) -> PlatformId {
        PlatformId(self as u16)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Platform::Unicode => "unicode",
            Platform::Macintosh => "macintosh",
            Platform::Iso => "iso",
            Platform::Windows => "windows",
            Platform::Custom => "custom",
        }
    }
}

impl Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Platform {
    type Err = UnknownPlatform;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "unicode" => Ok(Platform::Unicode),
            "macintosh" => Ok(Platform::Macintosh),
            "iso" => Ok(Platform::Iso),
            "windows" => Ok(Platform::Windows),
            "custom" => Ok(Platform::Custom),
            _ => Err(UnknownPlatform::new_str(s)),
        }
    }
}

impl TryFrom<PlatformId> for Platform {
    type Error = UnknownPlatform;

    fn try_from(value: PlatformId) -> Result<Self, Self::Error> {
        value.0.try_into()
    }
}

impl TryFrom<u16> for Platform {
    type Error = UnknownPlatform;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Platform::Unicode),
            1 => Ok(Platform::Macintosh),
            2 => Ok(Platform::Iso),
            3 => Ok(Platform::Windows),
            4 => Ok(Platform::Custom),
            _ => Err(UnknownPlatform::new_id(value)),
        }
    }
}

#[derive(Debug, Error)]
#[error("unknown platform {0}")]
pub struct UnknownPlatform(UnknownPlatformInner);

impl UnknownPlatform {
    fn new_str<S>(s: S) -> Self
    where
        S: Into<String>,
    {
        Self(UnknownPlatformInner::Str(s.into()))
    }

    fn new_id(id: u16) -> Self {
        Self(UnknownPlatformInner::Id(id))
    }
}

#[derive(Debug)]
enum UnknownPlatformInner {
    Str(String),
    Id(u16),
}

impl Display for UnknownPlatformInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Str(s) => write!(f, "unknown platform {s}"),
            Self::Id(id) => write!(f, "unknown platform ID {id}"),
        }
    }
}
