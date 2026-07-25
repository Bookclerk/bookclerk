//! Property-tag registry and evaluation.

use crate::compare::try_get_literal;
use crate::context::{BookContext, ChapterContext};
use crate::dotnet_format::{datetime_formatter, float_formatter, string_formatter};
use crate::items::{NameItem, SeriesItem, TagItem};
use crate::listformat::{finalize, formatted_list, ListItem, ListKind};
use crate::value::Value;

/// `(display_name, allows_format)` for every registered property tag.
pub fn property_tags() -> &'static [(&'static str, bool)] {
    &[
        ("id", false),
        ("title short", true),
        ("audible title", true),
        ("audible subtitle", true),
        ("title", true),
        ("first author", true),
        ("author", true),
        ("first narrator", true),
        ("narrator", true),
        ("first series", true),
        ("series#", true),
        ("series", true),
        ("minutes", true),
        ("bitrate", true),
        ("samplerate", true),
        ("channels", true),
        ("codec", true),
        ("publisher", true),
        ("categories", true),
        ("file version", true),
        ("bookclerk version", true),
        ("account nickname", true),
        ("account", true),
        ("first tag", true),
        ("tag", true),
        ("locale", true),
        ("year", true),
        ("language short", true),
        ("language", true),
        ("ui", true),
        ("os", true),
        ("file date", true),
        ("pub date", true),
        ("date added", true),
        ("ch count", true),
        ("ch title", true),
        ("ch# 0", false),
        ("ch#", true),
    ]
}

/// Remove all whitespace from a tag name for canonical, space-insensitive lookup.
pub(crate) fn canonical(name: &str) -> String {
    name.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Map legacy / alternate tag spellings onto their canonical tag name.
pub(crate) fn alias(canon: &str) -> &str {
    match canon {
        "asin" => "id",
        "authorfirst" => "firstauthor",
        "narratorfirst" => "firstnarrator",
        "chaptertitle" => "chtitle",
        "chapter#" => "ch#",
        "chapter#0" => "ch#0",
        "length" => "minutes",
        "fulltitle" => "title",
        "narrators" => "narrator",
        "authors" => "author",
        other => other,
    }
}

/// True if `canon` (already canonicalised + aliased) names a known tag.
pub(crate) fn is_known(canon: &str) -> bool {
    property_tags()
        .iter()
        .any(|(name, _)| canonical(name) == canon)
}

fn build_names(list: &[crate::context::Contributor]) -> Vec<NameItem> {
    list.iter().map(NameItem::new).collect()
}

fn list_display(items: &[NameItem], format: Option<&str>, kind: ListKind) -> String {
    let refs: Vec<&dyn ListItem> = items.iter().map(|i| i as &dyn ListItem).collect();
    finalize(&formatted_list(&refs, format, kind))
}

fn list_object(items: &[NameItem], format: Option<&str>, kind: ListKind) -> Value {
    let refs: Vec<&dyn ListItem> = items.iter().map(|i| i as &dyn ListItem).collect();
    Value::List(formatted_list(&refs, format, kind))
}

fn series_display(items: &[SeriesItem], format: Option<&str>) -> String {
    let refs: Vec<&dyn ListItem> = items.iter().map(|i| i as &dyn ListItem).collect();
    finalize(&formatted_list(&refs, format, ListKind::Series))
}

fn series_object(items: &[SeriesItem], format: Option<&str>) -> Value {
    let refs: Vec<&dyn ListItem> = items.iter().map(|i| i as &dyn ListItem).collect();
    Value::List(formatted_list(&refs, format, ListKind::Series))
}

fn tag_display(items: &[TagItem], format: Option<&str>) -> String {
    let refs: Vec<&dyn ListItem> = items.iter().map(|i| i as &dyn ListItem).collect();
    finalize(&formatted_list(&refs, format, ListKind::StringList))
}

fn tag_object(items: &[TagItem], format: Option<&str>) -> Value {
    let refs: Vec<&dyn ListItem> = items.iter().map(|i| i as &dyn ListItem).collect();
    Value::List(formatted_list(&refs, format, ListKind::StringList))
}

fn language_short(book: &BookContext) -> String {
    match &book.language {
        Some(l) => string_formatter(l, Some("3u")),
        None => String::new(),
    }
}

fn file_date(
    book: &BookContext,
    chapter: Option<&ChapterContext>,
) -> Option<chrono::NaiveDateTime> {
    chapter.and_then(|c| c.file_date).or(book.file_date)
}

/// Evaluate a tag to its display string. Returns `None` for unknown tags.
pub(crate) fn eval_display(
    name: &str,
    format: Option<&str>,
    book: &BookContext,
    chapter: Option<&ChapterContext>,
) -> Option<String> {
    let s = |opt: Option<String>| -> String {
        match opt {
            Some(v) => string_formatter(&v, format),
            None => String::new(),
        }
    };
    let n = |opt: Option<i64>| -> String {
        match opt {
            Some(v) => float_formatter(v as f64, format),
            None => String::new(),
        }
    };
    let d = |opt: Option<chrono::NaiveDateTime>| -> String {
        match opt {
            Some(v) => datetime_formatter(v, format),
            None => String::new(),
        }
    };

    Some(match name {
        "id" => book.isbn.clone(),
        "title" => s(book.full_title()),
        "titleshort" => s(book.title_short()),
        "audibletitle" => s(book.title.clone()),
        "audiblesubtitle" => s(book.subtitle.clone()),
        "author" => list_display(&build_names(&book.authors), format, ListKind::Name),
        "narrator" => list_display(&build_names(&book.narrators), format, ListKind::Name),
        "firstauthor" => match book.authors.first() {
            Some(c) => NameItem::new(c).to_string_fmt(format),
            None => String::new(),
        },
        "firstnarrator" => match book.narrators.first() {
            Some(c) => NameItem::new(c).to_string_fmt(format),
            None => String::new(),
        },
        "series" => {
            let items: Vec<SeriesItem> = book.series.iter().map(SeriesItem::new).collect();
            series_display(&items, format)
        }
        "firstseries" => match book.series.first() {
            Some(sd) => SeriesItem::new(sd).to_string_fmt(format),
            None => String::new(),
        },
        "series#" => match book.series.first() {
            Some(sd) => {
                let order = crate::series_order::SeriesOrder::parse(sd.order.as_deref());
                if order.is_empty() {
                    String::new()
                } else {
                    order.to_display(format)
                }
            }
            None => String::new(),
        },
        "minutes" => match book.length_minutes {
            Some(m) => minutes_display(m, format),
            None => String::new(),
        },
        "bitrate" => n(book.bitrate),
        "samplerate" => n(book.samplerate),
        "channels" => n(book.channels),
        "codec" => s(book.codec.clone()),
        "publisher" => s(book.publisher.clone()),
        "categories" => {
            let items: Vec<TagItem> = book.categories.iter().map(|t| TagItem::new(t)).collect();
            tag_display(&items, format)
        }
        "fileversion" => s(book.file_version.clone()),
        "bookclerkversion" => s(book.bookclerk_version.clone()),
        "account" => s(book.account.clone()),
        "accountnickname" => s(book.account_nickname.clone()),
        "tag" => {
            let items: Vec<TagItem> = book.tags.iter().map(|t| TagItem::new(t)).collect();
            tag_display(&items, format)
        }
        "firsttag" => s(book.tags.first().cloned()),
        "locale" => s(book.locale.clone()),
        "year" => n(book.year_published.map(i64::from)),
        "language" => s(book.language.clone()),
        "languageshort" => language_short(book),
        "ui" | "os" => String::new(),
        "filedate" => d(file_date(book, chapter)),
        "pubdate" => d(book.published_at),
        "dateadded" => d(book.purchased_at),
        "chcount" => n(chapter.map(|c| i64::from(c.chapter_count))),
        "chtitle" => match chapter.and_then(|c| c.chapter_title.clone()) {
            Some(t) => string_formatter(&t, format),
            None => String::new(),
        },
        "ch#" => n(chapter.map(|c| i64::from(c.chapter_number))),
        "ch#0" => match chapter {
            Some(c) => {
                let width = digits(c.chapter_count);
                format!("{:0>width$}", c.chapter_number, width = width)
            }
            None => String::new(),
        },
        _ => return None,
    })
}

/// Evaluate a tag to its object value (for conditionals). Returns `None` for unknown tags.
pub(crate) fn eval_object(
    name: &str,
    format: Option<&str>,
    book: &BookContext,
    chapter: Option<&ChapterContext>,
) -> Option<Value> {
    let s = |opt: Option<String>| -> Value {
        match opt {
            Some(v) => match format {
                Some(f) => Value::Str(string_formatter(&v, Some(f))),
                None => Value::Str(v),
            },
            None => Value::Null,
        }
    };
    let n = |opt: Option<i64>| -> Value {
        match opt {
            Some(v) => match format {
                Some(f) => Value::Str(float_formatter(v as f64, Some(f))),
                None => Value::Int(v),
            },
            None => Value::Null,
        }
    };
    let dt = |opt: Option<chrono::NaiveDateTime>| -> Value {
        match opt {
            Some(v) => match format {
                Some(f) => Value::Str(datetime_formatter(v, Some(f))),
                None => Value::Date(v),
            },
            None => Value::Null,
        }
    };

    Some(match name {
        "id" => Value::Str(book.isbn.clone()),
        "title" => s(book.full_title()),
        "titleshort" => s(book.title_short()),
        "audibletitle" => s(book.title.clone()),
        "audiblesubtitle" => s(book.subtitle.clone()),
        "author" => list_object(&build_names(&book.authors), format, ListKind::Name),
        "narrator" => list_object(&build_names(&book.narrators), format, ListKind::Name),
        "firstauthor" => match book.authors.first() {
            Some(c) => Value::Str(NameItem::new(c).to_string_fmt(format)),
            None => Value::Null,
        },
        "firstnarrator" => match book.narrators.first() {
            Some(c) => Value::Str(NameItem::new(c).to_string_fmt(format)),
            None => Value::Null,
        },
        "series" => {
            let items: Vec<SeriesItem> = book.series.iter().map(SeriesItem::new).collect();
            series_object(&items, format)
        }
        "firstseries" => match book.series.first() {
            Some(sd) => Value::Str(SeriesItem::new(sd).to_string_fmt(format)),
            None => Value::Null,
        },
        "series#" => match book.series.first() {
            Some(sd) => {
                let order = crate::series_order::SeriesOrder::parse(sd.order.as_deref());
                if order.is_empty() {
                    Value::Null
                } else {
                    Value::Str(order.to_display(format))
                }
            }
            None => Value::Null,
        },
        "minutes" => match book.length_minutes {
            Some(m) => match format {
                Some(f) => Value::Str(minutes_display(m, Some(f))),
                None => Value::Minutes(m),
            },
            None => Value::Null,
        },
        "bitrate" => n(book.bitrate),
        "samplerate" => n(book.samplerate),
        "channels" => n(book.channels),
        "codec" => s(book.codec.clone()),
        "publisher" => s(book.publisher.clone()),
        "categories" => {
            let items: Vec<TagItem> = book.categories.iter().map(|t| TagItem::new(t)).collect();
            tag_object(&items, format)
        }
        "fileversion" => s(book.file_version.clone()),
        "bookclerkversion" => s(book.bookclerk_version.clone()),
        "account" => s(book.account.clone()),
        "accountnickname" => s(book.account_nickname.clone()),
        "tag" => {
            let items: Vec<TagItem> = book.tags.iter().map(|t| TagItem::new(t)).collect();
            tag_object(&items, format)
        }
        "firsttag" => s(book.tags.first().cloned()),
        "locale" => s(book.locale.clone()),
        "year" => n(book.year_published.map(i64::from)),
        "language" => s(book.language.clone()),
        "languageshort" => {
            let ls = language_short(book);
            if ls.is_empty() {
                Value::Null
            } else {
                Value::Str(ls)
            }
        }
        "ui" | "os" => Value::Null,
        "filedate" => dt(file_date(book, chapter)),
        "pubdate" => dt(book.published_at),
        "dateadded" => dt(book.purchased_at),
        "chcount" => n(chapter.map(|c| i64::from(c.chapter_count))),
        "chtitle" => s(chapter.and_then(|c| c.chapter_title.clone())),
        "ch#" => n(chapter.map(|c| i64::from(c.chapter_number))),
        "ch#0" => match chapter {
            Some(c) => {
                let width = digits(c.chapter_count);
                Value::Str(format!("{:0>width$}", c.chapter_number, width = width))
            }
            None => Value::Null,
        },
        _ => return None,
    })
}

/// Resolve a conditional property reference (literal, or `tag[format]`) to a value.
pub(crate) fn resolve_property(
    property: &str,
    book: &BookContext,
    chapter: Option<&ChapterContext>,
) -> Value {
    if let Some(v) = try_get_literal(property) {
        return v;
    }
    let (name_part, format) = split_name_format(property);
    let canon = alias(&canonical(name_part)).to_string();
    eval_object(&canon, format.as_deref(), book, chapter).unwrap_or(Value::Null)
}

/// Split `name[format]` into `(name, format)` respecting `\` escapes.
fn split_name_format(s: &str) -> (&str, Option<String>) {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut byte_idx = 0;
    while i < chars.len() {
        if chars[i] == '[' {
            let name = &s[..byte_idx];
            // Read format until matching ']' (respecting escapes).
            let mut j = i + 1;
            let mut fmt = String::new();
            while j < chars.len() {
                if chars[j] == '\\' && j + 1 < chars.len() {
                    fmt.push(chars[j]);
                    fmt.push(chars[j + 1]);
                    j += 2;
                } else if chars[j] == ']' {
                    break;
                } else {
                    fmt.push(chars[j]);
                    j += 1;
                }
            }
            return (name.trim(), Some(fmt));
        }
        byte_idx += chars[i].len_utf8();
        i += 1;
    }
    (s.trim(), None)
}

fn digits(n: u32) -> usize {
    if n == 0 {
        1
    } else {
        (n as f64).log10().floor() as usize + 1
    }
}

/// Basic `<minutes>` formatting. Full .NET TimeSpan custom-format parity is a
/// known limitation; the default (total minutes) and simple cases are handled.
fn minutes_display(total_minutes: f64, format: Option<&str>) -> String {
    match format {
        None => format!("{}", total_minutes.round() as i64),
        Some(f) if f.trim().is_empty() => format!("{}", total_minutes.round() as i64),
        Some(_) => format!("{}", total_minutes.round() as i64),
    }
}
