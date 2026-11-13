//! Closed vocabularies of the library data model.
//!
//! Each enum is matched exhaustively in Rust, carried as a capnp enum on the
//! plugin wire, and stored as text under a `CHECK` constraint in SQL. The
//! [`as_str`](WorkKind::as_str) spellings are those stored values; a new
//! variant is appended to all three at once.

/// Declares a closed enum whose variants are stored as fixed text.
///
/// Generates the enum with its doc comments, `as_str`, `parse` and an
/// `ALL` list in declaration order, so the SQL `CHECK` lists and the
/// round-trip tests cover every variant.
macro_rules! stored_enum {
    (
        $(#[$meta:meta])*
        $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident => $text:literal, )+
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum $name {
            $( $(#[$vmeta])* $variant, )+
        }

        impl $name {
            /// Every variant, in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// The text stored for this value in SQL.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text,)+
                }
            }

            /// Parses a stored value; `None` for text that names no variant.
            #[must_use]
            pub fn parse(text: &str) -> Option<Self> {
                match text {
                    $($text => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

stored_enum! {
    /// What family of creative work a work is (`works.kind`).
    ///
    /// Whether a podcast work is a show or an episode is structural: an
    /// episode has a parent work (`works.parent_work_id`), a show has none.
    WorkKind {
        /// A book, in any expression and on any carrier.
        Book => "book",
        /// A podcast: the show, or one episode of it when it has a parent.
        Podcast => "podcast",
    }
}

stored_enum! {
    /// How a work is realised (`expressions.kind`).
    ///
    /// A translation is not a kind: the expression's language and its
    /// translator credits say it.
    ExpressionKind {
        /// The written text, as sold in print and ebook editions.
        Text => "text",
        /// A reading of the text by one or more narrators.
        Narration => "narration",
        /// A full-cast dramatisation or performance.
        Dramatisation => "dramatisation",
        /// Sequential art: comics, graphic novels, manga.
        Graphic => "graphic",
        /// Audio that is itself the first realisation: podcast episodes,
        /// lectures, single-voice originals.
        OriginalAudio => "original_audio",
    }
}

stored_enum! {
    /// Carrier of an edition (`editions.media_kind`).
    MediaKind {
        /// Downloadable audio: audiobooks and podcast episodes.
        Audio => "audio",
        /// A downloadable document: EPUB, PDF, CBZ and similar.
        Ebook => "ebook",
        /// A printed edition.
        Print => "print",
    }
}

stored_enum! {
    /// Whether an expression is abridged (`expressions.abridgement`).
    Abridgement {
        /// The source did not say. Never matches any value, itself included.
        Unknown => "unknown",
        /// The full text.
        Unabridged => "unabridged",
        /// A shortened text.
        Abridged => "abridged",
    }
}

stored_enum! {
    /// Scheme of an external identifier; it fixes level and strength.
    IdentifierScheme {
        /// ISBN, normalised to ISBN-13; a weak edition identifier.
        Isbn => "isbn",
        /// Amazon ASIN in one marketplace; a strong edition identifier.
        Asin => "asin",
        /// OpenLibrary edition id (`OL…M`); a strong edition identifier.
        OpenLibraryEdition => "openlibrary_edition",
        /// OpenLibrary work id (`OL…W`); names a work.
        OpenLibraryWork => "openlibrary_work",
    }
}

stored_enum! {
    /// Whether an edition identifier names exactly one edition.
    Strength {
        /// Names exactly one edition; unique across the library.
        Strong => "strong",
        /// Shared in practice; only proposes a candidate edition.
        Weak => "weak",
    }
}

stored_enum! {
    /// The bibliographic level a credit or identifier attaches to.
    Level {
        /// The work.
        Work => "work",
        /// The expression.
        Expression => "expression",
        /// The edition.
        Edition => "edition",
    }
}

stored_enum! {
    /// What a contributor did (`credits.role`).
    CreditRole {
        /// Wrote the work.
        Author => "author",
        /// Reads the text in a narration.
        Narrator => "narrator",
        /// Translated the text.
        Translator => "translator",
        /// Edited the work or edition.
        Editor => "editor",
        /// Illustrated the work or edition.
        Illustrator => "illustrator",
        /// Published the edition.
        Publisher => "publisher",
        /// Hosts a podcast.
        Host => "host",
        /// Performs a part in a dramatisation.
        Cast => "cast",
        /// Adapted the work into a dramatisation.
        Adaptor => "adaptor",
    }
}

stored_enum! {
    /// How a holder came to own a listing (`holdings.entitlement`).
    Entitlement {
        /// Bought outright.
        Purchase => "purchase",
        /// Included with a store membership while it lasts.
        Plus => "plus",
        /// Borrowed for a time.
        Loan => "loan",
        /// Given away by the store.
        Free => "free",
    }
}

impl IdentifierScheme {
    /// The level whose identifier table this scheme lands in.
    #[must_use]
    pub const fn level(self) -> Level {
        match self {
            Self::Isbn | Self::Asin | Self::OpenLibraryEdition => Level::Edition,
            Self::OpenLibraryWork => Level::Work,
        }
    }

    /// Whether an edition identifier of this scheme is strong or weak.
    ///
    /// Work identifiers report [`Strength::Strong`]: one names one work.
    #[must_use]
    pub const fn strength(self) -> Strength {
        match self {
            Self::Isbn => Strength::Weak,
            Self::Asin | Self::OpenLibraryEdition | Self::OpenLibraryWork => Strength::Strong,
        }
    }

    /// Whether a value of this scheme is scoped to a marketplace.
    #[must_use]
    pub const fn marketplace_scoped(self) -> bool {
        match self {
            Self::Asin => true,
            Self::Isbn | Self::OpenLibraryEdition | Self::OpenLibraryWork => false,
        }
    }
}

impl CreditRole {
    /// The level this role attaches to unless a source says otherwise.
    #[must_use]
    pub const fn default_level(self) -> Level {
        match self {
            Self::Author | Self::Adaptor => Level::Work,
            Self::Narrator | Self::Translator | Self::Host | Self::Cast => Level::Expression,
            Self::Editor | Self::Illustrator | Self::Publisher => Level::Edition,
        }
    }
}

impl MediaKind {
    /// Whether the host acquires editions of this carrier today.
    #[must_use]
    pub const fn acquirable(self) -> bool {
        match self {
            Self::Audio => true,
            Self::Ebook | Self::Print => false,
        }
    }
}

/// SQL `CHECK` list of an enum's stored values: `'a', 'b', 'c'`.
///
/// # Arguments
///
/// * `values` - The stored spellings, usually `Enum::ALL` mapped through `as_str`.
///
/// # Returns
///
/// The comma-separated, single-quoted list for an `IN (…)` clause.
#[must_use]
pub fn sql_check_list(values: impl IntoIterator<Item = &'static str>) -> String {
    values
        .into_iter()
        .map(|v| format!("'{v}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Checks that every variant of `$ty` parses back from its stored text.
    macro_rules! round_trip {
        ($ty:ty) => {
            for v in <$ty>::ALL {
                assert_eq!(<$ty>::parse(v.as_str()), Some(*v));
                assert_eq!(v.to_string(), v.as_str());
            }
            assert_eq!(<$ty>::parse("nope"), None);
        };
    }

    #[test]
    fn stored_values_round_trip() {
        round_trip!(WorkKind);
        round_trip!(ExpressionKind);
        round_trip!(MediaKind);
        round_trip!(Abridgement);
        round_trip!(IdentifierScheme);
        round_trip!(Strength);
        round_trip!(Level);
        round_trip!(CreditRole);
        round_trip!(Entitlement);
    }

    #[test]
    fn schemes_fix_level_strength_and_scope() {
        assert_eq!(IdentifierScheme::Isbn.strength(), Strength::Weak);
        assert_eq!(IdentifierScheme::Asin.strength(), Strength::Strong);
        assert!(IdentifierScheme::Asin.marketplace_scoped());
        assert!(!IdentifierScheme::Isbn.marketplace_scoped());
        assert_eq!(IdentifierScheme::OpenLibraryWork.level(), Level::Work);
        assert_eq!(IdentifierScheme::OpenLibraryEdition.level(), Level::Edition);
    }

    #[test]
    fn roles_default_to_their_usual_level() {
        assert_eq!(CreditRole::Author.default_level(), Level::Work);
        assert_eq!(CreditRole::Narrator.default_level(), Level::Expression);
        assert_eq!(CreditRole::Publisher.default_level(), Level::Edition);
        assert!(MediaKind::Audio.acquirable());
        assert!(!MediaKind::Print.acquirable());
    }

    #[test]
    fn check_lists_quote_each_value() {
        let list = sql_check_list(Abridgement::ALL.iter().map(|a| a.as_str()));
        assert_eq!(list, "'unknown', 'unabridged', 'abridged'");
    }
}
