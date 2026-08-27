use crate::catalog::model::FileType;
use crate::errors::DomainError;

/// The file types one index run records (UC-01). Absent means every supported
/// type, which is why the empty scope is [`IndexScope::all`] rather than a
/// scope of nothing: every caller that predates this parameter sends nothing,
/// and reading that as "index nothing" would turn a missing argument into a
/// run that does no work at all.
///
/// It is carried on the request and passed to the walk rather than stored on
/// the run. A scope matters only while the run walks, and a column recording
/// it would exist to answer a question nobody has asked yet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexScope {
    /// `None` is every type, not an empty selection — see the type's own doc
    /// comment. `Vec` rather than a set: a scope holds at most one entry per
    /// `FileType` and is read once per scanned file, so the linear scan is
    /// over seven elements at worst and needs no hashing to beat.
    types: Option<Vec<FileType>>,
}

impl IndexScope {
    /// Every supported type — what an absent scope means.
    pub fn all() -> Self {
        Self { types: None }
    }

    /// Parse a scope from the wire names [`FileType::as_str`] writes, so a
    /// client spells a type the same way it reads one back (FR-FC-24).
    ///
    /// An unrecognised name is [`DomainError::InvalidInput`], deliberately
    /// unlike the priority parsers, which fall back to `normal`. A scope has
    /// no safe fallback: the only candidate is "every type", which is the
    /// *opposite* of what a caller asking for a narrower scope wants, and it
    /// fails in the direction of cataloguing exactly the files the owner
    /// meant to exclude. Better the caller learns at the call than discovers
    /// it in a library full of cover art.
    ///
    /// An empty list is [`IndexScope::all`], for the reason absent is. Blank
    /// names are dropped rather than refused: they are what a trailing
    /// separator in the FFI surface's comma-separated list produces, and a
    /// separator is not a misspelt type.
    pub fn parse<I, S>(names: I) -> Result<Self, DomainError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut types: Vec<FileType> = Vec::new();
        for name in names {
            let name = name.as_ref().trim();
            if name.is_empty() {
                continue;
            }
            let file_type = FileType::from_wire(name).ok_or_else(|| {
                DomainError::InvalidInput(format!(
                    "unknown file type {name:?}; expected one of audio, video, html, text, \
                     document, comic, image"
                ))
            })?;
            // Deduplicated so a scope that names a type twice reads the same
            // as one that names it once — the answer `includes` gives is
            // identical either way, and a caller cannot tell the difference.
            if !types.contains(&file_type) {
                types.push(file_type);
            }
        }
        Ok(if types.is_empty() {
            Self::all()
        } else {
            Self { types: Some(types) }
        })
    }

    /// Parse one comma-separated list of wire names — the form the FFI's
    /// `types` argument arrives in and the form `catalog_runs.scope` stores.
    ///
    /// The single place that encoding is read, so the boundary that accepts a
    /// scope and the column that keeps it cannot drift apart on what a comma
    /// means. [`IndexScope::to_wire`] is its inverse.
    pub fn parse_list(raw: &str) -> Result<Self, DomainError> {
        Self::parse(raw.split(','))
    }

    /// The scope as the one comma-separated string `catalog_runs.scope`
    /// stores — `"audio"`, `"audio,image"` — or `None` for every type, which
    /// is that column's NULL.
    ///
    /// `None` rather than an empty string on purpose: NULL is what a row
    /// written before the column existed already holds, so an unscoped run
    /// and a legacy row read back identically and neither needs a backfill.
    pub fn to_wire(&self) -> Option<String> {
        self.types.as_ref().map(|types| {
            types
                .iter()
                .map(|file_type| file_type.as_str())
                .collect::<Vec<_>>()
                .join(",")
        })
    }

    /// Whether a file the classifier resolved to `file_type` is one this run
    /// records. Always true for [`IndexScope::all`].
    pub fn includes(&self, file_type: FileType) -> bool {
        match &self.types {
            None => true,
            Some(types) => types.contains(&file_type),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_no_names_when_parsed_then_every_type_is_included() {
        let scope = IndexScope::parse(Vec::<String>::new()).expect("parse");

        assert_eq!(scope, IndexScope::all());
        assert!(scope.includes(FileType::Audio));
        assert!(scope.includes(FileType::Image));
    }

    #[test]
    fn given_one_name_when_parsed_then_only_that_type_is_included() {
        let scope = IndexScope::parse(["audio"]).expect("parse");

        assert!(scope.includes(FileType::Audio));
        assert!(!scope.includes(FileType::Image));
        assert!(!scope.includes(FileType::Video));
    }

    #[test]
    fn given_every_wire_name_when_parsed_then_it_maps_to_its_type() {
        for file_type in [
            FileType::Audio,
            FileType::Video,
            FileType::Html,
            FileType::Text,
            FileType::Document,
            FileType::Comic,
            FileType::Image,
        ] {
            let scope = IndexScope::parse([file_type.as_str()]).expect("parse");
            assert!(
                scope.includes(file_type),
                "{} must parse from the name as_str writes",
                file_type.as_str()
            );
        }
    }

    #[test]
    fn given_an_unknown_name_when_parsed_then_invalid_input() {
        let result = IndexScope::parse(["audio", "sculpture"]);

        assert!(matches!(result, Err(DomainError::InvalidInput(_))));
    }

    #[test]
    fn given_blank_names_when_parsed_then_they_are_dropped() {
        let scope = IndexScope::parse(["audio", "", "  "]).expect("parse");

        assert!(scope.includes(FileType::Audio));
        assert!(!scope.includes(FileType::Text));
    }

    /// The column and the FFI argument share one encoding, so what is written
    /// is what parses back — otherwise a resumed run would walk a scope its
    /// start never asked for.
    #[test]
    fn given_a_scope_when_written_to_wire_then_it_parses_back_the_same() {
        let scope = IndexScope::parse(["audio", "image"]).expect("parse");

        let wire = scope.to_wire().expect("a narrowed scope has a wire form");

        assert_eq!(wire, "audio,image");
        assert_eq!(IndexScope::parse_list(&wire).expect("parse back"), scope);
    }

    #[test]
    fn given_every_type_when_written_to_wire_then_none() {
        assert_eq!(IndexScope::all().to_wire(), None);
    }

    #[test]
    fn given_a_repeated_name_when_parsed_then_it_reads_as_one() {
        let scope = IndexScope::parse(["audio", "audio"]).expect("parse");

        assert_eq!(scope, IndexScope::parse(["audio"]).expect("parse"));
    }
}
