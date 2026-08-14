//! `ListItem` / `FormatItem` implementations for contributors, series and tags.

use crate::context::{Contributor, Series};
use crate::listformat::ListItem;
use crate::nameparser::HumanName;
use crate::series_order::SeriesOrder;
use crate::template_string::{self, FormatItem, ItemValue};

/// Default contributor format: title, first, middle, last, suffix (`{T} {F} {M} {L} {S}`).
const NAME_DEFAULT_FORMAT: &str = "{T} {F} {M} {L} {S}";

/// A parsed contributor ready for `{T}{F}{M}{L}{S}{ID}` formatting.
pub(crate) struct NameItem {
    /// Parsed personal name used by `{T}{F}{M}{L}{S}` tokens.
    human: HumanName,
    /// Optional contributor id substituted for `{ID}`.
    id: Option<String>,
}

impl NameItem {
    /// Parses a contributor display name and keeps its optional storefront id.
    pub fn new(c: &Contributor) -> Self {
        Self {
            human: HumanName::parse(&c.name),
            id: c.id.clone(),
        }
    }

    /// Resolves one format token; `{L}`/`{F}` swap when the parsed last name is empty.
    fn field(&self, token: &str) -> String {
        let last_empty = self.human.last.trim().is_empty();
        match token {
            "L" => {
                if last_empty {
                    self.human.first.clone()
                } else {
                    self.human.last.clone()
                }
            }
            "F" => {
                if last_empty {
                    self.human.last.clone()
                } else {
                    self.human.first.clone()
                }
            }
            "T" => self.human.title.clone(),
            "M" => self.human.middle.clone(),
            "S" => self.human.suffix.clone(),
            "ID" => self.id.clone().unwrap_or_default(),
            _ => String::new(),
        }
    }
}

impl FormatItem for NameItem {
    fn lookup(&self, token: &str) -> Option<ItemValue> {
        match token {
            "L" | "F" | "T" | "M" | "S" | "ID" => Some(ItemValue::Str(self.field(token))),
            _ => None,
        }
    }
}

impl ListItem for NameItem {
    fn to_string_fmt(&self, format: Option<&str>) -> String {
        let template = match format {
            Some(f) if !f.trim().is_empty() => f,
            _ => NAME_DEFAULT_FORMAT,
        };
        template_string::format(self, template)
    }

    fn sort_key(&self, token: &str) -> String {
        self.field(token)
    }
}

/// A series entry for `{N}{#}{ID}` formatting.
pub(crate) struct SeriesItem {
    /// Series display name for `{N}`.
    name: String,
    /// Parsed series index for `{#}` (mixed text and numbers).
    order: SeriesOrder,
    /// Optional series id substituted for `{ID}`.
    id: Option<String>,
}

impl SeriesItem {
    /// Copies series name/id and parses the order string into [`SeriesOrder`].
    pub fn new(s: &Series) -> Self {
        Self {
            name: s.name.clone(),
            order: SeriesOrder::parse(s.order.as_deref()),
            id: s.id.clone(),
        }
    }
}

impl FormatItem for SeriesItem {
    fn lookup(&self, token: &str) -> Option<ItemValue> {
        match token {
            "N" => Some(ItemValue::Str(self.name.clone())),
            "#" => Some(ItemValue::Series(self.order.clone())),
            "ID" => Some(ItemValue::Str(self.id.clone().unwrap_or_default())),
            _ => None,
        }
    }
}

impl ListItem for SeriesItem {
    fn to_string_fmt(&self, format: Option<&str>) -> String {
        match format {
            Some(f) if !f.trim().is_empty() => template_string::format(self, f),
            _ => self.name.trim().to_string(),
        }
    }

    fn sort_key(&self, token: &str) -> String {
        match token {
            "N" => self.name.clone(),
            "#" => self.order.to_display(None),
            "ID" => self.id.clone().unwrap_or_default(),
            _ => String::new(),
        }
    }
}

/// A tag / string-list entry for `{S}` formatting.
pub(crate) struct TagItem {
    /// Tag or list-entry text substituted for `{S}`.
    value: String,
}

impl TagItem {
    /// Wraps a tag / string-list entry for `{S}` formatting.
    pub fn new(value: &str) -> Self {
        Self {
            value: value.to_string(),
        }
    }
}

impl FormatItem for TagItem {
    fn lookup(&self, token: &str) -> Option<ItemValue> {
        match token {
            "S" => Some(ItemValue::Str(self.value.clone())),
            _ => None,
        }
    }
}

impl ListItem for TagItem {
    fn to_string_fmt(&self, format: Option<&str>) -> String {
        match format {
            Some(f) if !f.trim().is_empty() => template_string::format(self, f),
            _ => self.value.clone(),
        }
    }

    fn sort_key(&self, token: &str) -> String {
        match token {
            "S" => self.value.clone(),
            _ => String::new(),
        }
    }
}
