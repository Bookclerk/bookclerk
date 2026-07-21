//! Port of key cases from Libation's `TemplatesTests.cs`.
//!
//! The fixture mirrors `TemplatesTests.Shared.GetLibraryBook()` so the expected
//! values can be copied directly from the upstream `[DataRow]` attributes.

use chrono::{NaiveDate, NaiveDateTime};
use libation_naming::{
    expand_filename, expand_template, BookContext, ChapterContext, ContentKind, Contributor,
    ReplacementRule, Series,
};

fn dt(y: i32, m: u32, d: u32) -> NaiveDateTime {
    NaiveDate::from_ymd_opt(y, m, d)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap()
}

/// Equivalent of `GetLibraryBook()` (single "Sherlock Holmes" series).
fn book() -> BookContext {
    BookContext {
        isbn: "asin".into(),
        title: Some("A Study in Scarlet: A Sherlock Holmes Novel".into()),
        subtitle: Some("An Audible Original Drama".into()),
        title_with_subtitle: Some("A Study in Scarlet: An Audible Original Drama".into()),
        authors: vec![
            Contributor::new("Arthur Conan Doyle", Some("B000AQ43GQ".into())),
            Contributor::new("Stephen Fry - introductions", Some("B000APAGVS".into())),
        ],
        narrators: vec![],
        series: vec![Series::new(
            "Sherlock Holmes",
            Some("1".into()),
            Some("B08376S3R2".into()),
        )],
        tags: vec!["Tag1".into(), "Tag2".into(), "Tag3".into()],
        account: Some("myaccount@example.co".into()),
        account_nickname: Some("my account".into()),
        locale: Some("us".into()),
        language: Some("English".into()),
        year_published: None,
        published_at: Some(dt(2017, 2, 27)),
        purchased_at: Some(dt(2022, 6, 9)),
        file_date: Some(dt(2023, 1, 28)),
        length_minutes: Some(100.0),
        bitrate: Some(128),
        samplerate: Some(44100),
        channels: Some(2),
        codec: Some(r"AAC[LC]\MP3".into()),
        is_abridged: true,
        content_kind: ContentKind::Book,
        publisher: None,
        categories: vec![],
        libation_version: Some(String::new()),
        file_version: None,
    }
}

/// The seven-author fixture used by `NameFormat_formatters`.
fn book_many_authors() -> BookContext {
    let mut b = book();
    b.authors = vec![
        Contributor::new("Jill Conner Browne", Some("B1".into())),
        Contributor::new("Charles E. Gannon", Some("B2".into())),
        Contributor::new("Christopher John Fetherolf", Some("B3".into())),
        Contributor::new("Lucy Maud Montgomery", Some("B4".into())),
        Contributor::new("Jon Bon Jovi", Some("B5".into())),
        Contributor::new("Paul Van Doren", Some("B6".into())),
        Contributor::new("Emma Gannon", Some("B7".into())),
    ];
    b
}

fn eval(template: &str) -> String {
    expand_template(template, &book(), None).unwrap()
}

fn eval_book(template: &str, b: &BookContext) -> String {
    expand_template(template, b, None).unwrap()
}

/// Evaluate as a filename component (whitespace-collapsed like `GetFilename`).
fn eval_file(template: &str) -> String {
    expand_filename(template, &book(), None, &[]).unwrap()
}

fn eval_file_book(template: &str, b: &BookContext) -> String {
    expand_filename(template, b, None, &[]).unwrap()
}

// ---------------------------------------------------------------------------
// Basic tag replacement
// ---------------------------------------------------------------------------

#[test]
fn tag_replacement() {
    assert_eq!(eval("<id>"), "asin");
    assert_eq!(
        eval("<bitrate> - <samplerate> - <channels>"),
        "128 - 44100 - 2"
    );
    assert_eq!(eval("<bitrate>Kbps <samplerate>Hz"), "128Kbps 44100Hz");
    assert_eq!(
        eval("<title>"),
        "A Study in Scarlet: An Audible Original Drama"
    );
    assert_eq!(eval("<titleshort>"), "A Study in Scarlet");
    assert_eq!(
        eval("<audible title>"),
        "A Study in Scarlet: A Sherlock Holmes Novel"
    );
    assert_eq!(eval("<audible subtitle>"), "An Audible Original Drama");
}

// ---------------------------------------------------------------------------
// FormatTags: number / string formatters
// ---------------------------------------------------------------------------

#[test]
fn format_tags() {
    assert_eq!(eval("<bitrate>Kbps <samplerate>Hz"), "128Kbps 44100Hz");
    assert_eq!(eval("<bitrate>Kbps <samplerate[6]>Hz"), "128Kbps 044100Hz");
    assert_eq!(eval("<bitrate[1]>Kbps <samplerate>Hz"), "128Kbps 44100Hz");
    assert_eq!(
        eval("<bitrate[2]>Kbps <titleshort[u]>"),
        "128Kbps A STUDY IN SCARLET"
    );
    assert_eq!(
        eval("<bitrate[3]>Kbps <titleshort[t]>"),
        "128Kbps A Study In Scarlet"
    );
    assert_eq!(
        eval("<bitrate[4]>Kbps <titleshort[l]>"),
        "0128Kbps a study in scarlet"
    );
    assert_eq!(
        eval(r"<bitrate[00'['0\#0']']>Kbps <titleshort[T]>"),
        "01[2#8]Kbps A Study In Scarlet"
    );
    assert_eq!(eval("<codec[7t]> <samplerate[6]>Hz"), "Aac[Lc] 044100Hz");
    assert_eq!(eval("<codec[3T]> <titleshort[ 5 U ]>"), "AAC A STU");
    assert_eq!(
        eval("<bitrate  [ 4 ]  >Kbps <samplerate   [  6  ]   >Hz"),
        "0128Kbps 044100Hz"
    );
}

// ---------------------------------------------------------------------------
// Empty fields
// ---------------------------------------------------------------------------

#[test]
fn empty_fields() {
    assert_eq!(eval("<narrator>"), "");
    assert_eq!(eval("<narrator[format({L})]>"), "");
    assert_eq!(eval("<narrator[count()]>"), "");
    assert_eq!(eval("<first narrator>"), "");
    assert_eq!(eval("<file version>"), "");
    assert_eq!(eval("<libation version>"), "");
    assert_eq!(eval("<year>"), "");
}

// ---------------------------------------------------------------------------
// Date formatting
// ---------------------------------------------------------------------------

#[test]
fn date_format() {
    // File-level cases (whitespace-collapsed like GetFilename).
    assert_eq!(eval_file("<id> - <filedate[yy-MM-dd]>"), "asin - 23-01-28");
    assert_eq!(
        eval_file("<id> - <filedate  [  yy-MM-dd    ]  >"),
        "asin - 23-01-28"
    );
    assert_eq!(
        eval_file("<id> - <file date  [yy-MM-dd]  >"),
        "asin - 23-01-28"
    );
    assert_eq!(
        eval_file("<id> - <file     date[yy-MM-dd]>"),
        "asin - 23-01-28"
    );
    assert_eq!(eval_file("<id> - <file date[]>"), "asin - 2023-01-28");
    assert_eq!(eval_file("<id> - <filedate>"), "asin - 2023-01-28");
    assert_eq!(eval_file("<id> - <file date>"), "asin - 2023-01-28");
    assert_eq!(
        eval_file("<filedate[yy-MM-dd]> <date added[yy-MM-dd]> <pubdate[yy-MM]>"),
        "23-01-28 22-06-09 17-02"
    );
}

// ---------------------------------------------------------------------------
// removeSpaces: replacement + whitespace collapsing
// ---------------------------------------------------------------------------

#[test]
fn remove_spaces() {
    // Replacements turning digits into double spaces, then collapsed on output.
    let rules = vec![
        ReplacementRule {
            find: "4".into(),
            replace: "  ".into(),
        },
        ReplacementRule {
            find: "2".into(),
            replace: "  ".into(),
        },
    ];
    let f = |t: &str| expand_filename(t, &book(), None, &rules).unwrap();

    // samplerate 44100 -> "    100" -> "100"
    assert_eq!(f("<samplerate>"), "100");
    assert_eq!(f(" <samplerate> "), "100");
    assert_eq!(f("4<samplerate>4"), "100");
    // bitrate 128 -> "1  8" -> "1 8"
    assert_eq!(f("<bitrate>   -   <bitrate>"), "1 8 - 1 8");
    assert_eq!(f(" <bitrate> - <bitrate> "), "1 8 - 1 8");
    assert_eq!(
        f("<channels><channels><samplerate><channels><channels>"),
        "100"
    );
    assert_eq!(
        f(" <channels> - <channels> <samplerate> <channels> - <channels>"),
        "- 100 -"
    );
}

// ---------------------------------------------------------------------------
// NameFormat: name parsing ({T}{F}{M}{L}{S})
// ---------------------------------------------------------------------------

fn name_fmt(author: &str) -> String {
    let mut b = book();
    b.authors = vec![Contributor::new(author, None)];
    eval_book(
        "<author[format(Title={T}, First={F}, Middle={M} Last={L}, Suffix={S})]>",
        &b,
    )
}

#[test]
fn name_format_unusual() {
    assert_eq!(
        name_fmt("Bruce Bueno de Mesquita"),
        "Title=, First=Bruce, Middle=Bueno Last=de Mesquita, Suffix="
    );
    assert_eq!(
        name_fmt("Ramon de Ocampo"),
        "Title=, First=Ramon, Middle= Last=de Ocampo, Suffix="
    );
    assert_eq!(
        name_fmt("Carla Naumburg PhD"),
        "Title=, First=Carla, Middle= Last=Naumburg, Suffix=PhD"
    );
    assert_eq!(
        name_fmt("Tamara Lovatt-Smith"),
        "Title=, First=Tamara, Middle= Last=Lovatt-Smith, Suffix="
    );
    assert_eq!(
        name_fmt("Common"),
        "Title=, First=, Middle= Last=Common, Suffix="
    );
    assert_eq!(
        name_fmt("Doug Tisdale Jr."),
        "Title=, First=Doug, Middle= Last=Tisdale, Suffix=Jr"
    );
    assert_eq!(
        name_fmt("Robert S. Mueller III"),
        "Title=, First=Robert, Middle=S. Last=Mueller, Suffix=III"
    );
    assert_eq!(
        name_fmt("Patrick O'Brian"),
        "Title=, First=Patrick, Middle= Last=O'Brian, Suffix="
    );
    assert_eq!(
        name_fmt("Marine Le Pen"),
        "Title=, First=Marine, Middle= Last=Le Pen, Suffix="
    );
}

// ---------------------------------------------------------------------------
// NameFormat: list formatters (sort, slice, max, unique, filter, count, format)
// ---------------------------------------------------------------------------

fn author_fmt(template: &str) -> String {
    eval_book(template, &book_many_authors())
}

#[test]
fn name_format_list() {
    assert_eq!(
        author_fmt("<author>"),
        "Jill Conner Browne, Charles E. Gannon, Christopher John Fetherolf, Lucy Maud Montgomery, Jon Bon Jovi, Paul Van Doren, Emma Gannon"
    );
    assert_eq!(
        author_fmt("<author[sort(F)]>"),
        "Charles E. Gannon, Christopher John Fetherolf, Emma Gannon, Jill Conner Browne, Jon Bon Jovi, Lucy Maud Montgomery, Paul Van Doren"
    );
    assert_eq!(
        author_fmt("<author  [  max(  1  )  ]>"),
        "Jill Conner Browne"
    );
    assert_eq!(
        author_fmt("<author[max(2)]>"),
        "Jill Conner Browne, Charles E. Gannon"
    );
    assert_eq!(
        author_fmt("<author[slice(3)]>"),
        "Christopher John Fetherolf"
    );
    assert_eq!(
        author_fmt("<author[slice(3...5)]>"),
        "Christopher John Fetherolf, Lucy Maud Montgomery, Jon Bon Jovi"
    );
    assert_eq!(author_fmt("<author[slice(-2)]>"), "Paul Van Doren");
    assert_eq!(
        author_fmt("<author[slice(-3..-2)]>"),
        "Jon Bon Jovi, Paul Van Doren"
    );
    assert_eq!(
        author_fmt("<author[unique({L:1}) format({L})]>"),
        "Browne, Gannon, Fetherolf, Montgomery, Van Doren"
    );
    assert_eq!(author_fmt("<author[filter({L:1} = 'B') count()]>"), "2");
    assert_eq!(
        author_fmt("<author[filter({F:1}~{L}~'J~B') format({L})]>"),
        "Browne, Bon Jovi"
    );
    assert_eq!(
        author_fmt(r"<author[filter({F:1}\'{L:1} = 'J''B') format({L})]>"),
        "Browne, Bon Jovi"
    );
    assert_eq!(author_fmt("<author[filter(<'99') count()]>"), "");
    assert_eq!(author_fmt("<author[filter(<26) count()]>"), "6");
    assert_eq!(author_fmt("<author[count()]>"), "7");
    assert_eq!(author_fmt("<author[max(42) count()]>"), "7");
    assert_eq!(author_fmt("<author[max(2) count()]>"), "2");
    assert_eq!(author_fmt("<author[count(000)]>"), "007");
    assert_eq!(
        author_fmt("<author[format({L}, {F})]>"),
        "Browne, Jill, Gannon, Charles, Fetherolf, Christopher, Montgomery, Lucy, Bon Jovi, Jon, Van Doren, Paul, Gannon, Emma"
    );
    assert_eq!(
        author_fmt("<author[format({ID})]>"),
        "B1, B2, B3, B4, B5, B6, B7"
    );
    assert_eq!(
        author_fmt("<author[format({L}, {F}) separator( - ) max(3)]>"),
        "Browne, Jill - Gannon, Charles - Fetherolf, Christopher"
    );
    assert_eq!(
        author_fmt("<author[sort(F) max(2) separator(; ) format({F})]>"),
        "Charles; Christopher"
    );
    assert_eq!(author_fmt("<first author>"), "Jill Conner Browne");
    assert_eq!(author_fmt("<first author[{L}, {F}]>"), "Browne, Jill");
}

// ---------------------------------------------------------------------------
// Series formatters + series# padding
// ---------------------------------------------------------------------------

fn book_series(order: &str) -> BookContext {
    let mut b = book();
    b.series = vec![Series::new(
        "Series A",
        Some(order.into()),
        Some("B1".into()),
    )];
    b
}

#[test]
fn series_order_padding() {
    assert_eq!(eval_book("<first series[{#}]>", &book_series("1-6")), "1-6");
    assert_eq!(
        eval_book("<series[format({#:F2})]>", &book_series("1-6")),
        "1.00-6.00"
    );
    assert_eq!(
        eval_book("<first series[{#:F2}]>", &book_series("1-6")),
        "1.00-6.00"
    );
    assert_eq!(eval_book("<series#[F2]>", &book_series("1-6")), "1.00-6.00");
    assert_eq!(
        eval_file_book("<series#[F2]>", &book_series("front 1-6 back")),
        "front 1.00-6.00 back"
    );
    assert_eq!(
        eval_file_book("<series#[F2]>", &book_series("front    1 - 6    back")),
        "front 1.00 - 6.00 back"
    );
    assert_eq!(eval_book("<series#[F2]>", &book_series("f.1")), "f.1.00");
    assert_eq!(eval_book("<series#[F2]>", &book_series("f1g")), "f1.00g");
    assert_eq!(eval_book("<series#[]>", &book_series("1")), "1");
    assert_eq!(eval_book("<series#>", &book_series("1")), "1");
    assert_eq!(eval_file_book("<series#>", &book_series(" 1 6 ")), "1 6");
}

fn book_series_multi() -> BookContext {
    let mut b = book();
    b.series = vec![
        Series::new("Series A", Some("1".into()), Some("B1".into())),
        Series::new("Series B", Some("6".into()), Some("B2".into())),
        Series::new("Series C", Some("2".into()), Some("B3".into())),
        Series::new("Series D", Some("1-5".into()), Some("B4".into())),
    ];
    b
}

#[test]
fn series_format() {
    let b = book_series_multi();
    assert_eq!(
        eval_book("<series>", &b),
        "Series A, Series B, Series C, Series D"
    );
    assert_eq!(eval_book("<series[slice(2..3)]>", &b), "Series B, Series C");
    assert_eq!(eval_book("<series[count(00)]>", &b), "04");
    assert_eq!(eval_book("<series[max(2)]>", &b), "Series A, Series B");
    assert_eq!(
        eval_book("<series[format({N}, {#}, {ID}) separator(; )]>", &b),
        "Series A, 1, B1; Series B, 6, B2; Series C, 2, B3; Series D, 1-5, B4"
    );
    assert_eq!(eval_book("<first series>", &b), "Series A");
    assert_eq!(
        eval_book("<first series[{N}, {#}, {ID}]>", &b),
        "Series A, 1, B1"
    );
}

// ---------------------------------------------------------------------------
// Conditionals: has / is / cmp
// ---------------------------------------------------------------------------

#[test]
fn has_value_on_empty() {
    assert_eq!(eval("<has libation version->empty-string<-has>"), "");
    assert_eq!(
        eval("<!has libation version->empty-string<-has>"),
        "empty-string"
    );
    assert_eq!(eval("<is libation version[=foobar]->empty-string<-is>"), "");
    assert_eq!(
        eval("<is libation version[=]->empty-string<-is>"),
        "empty-string"
    );
    assert_eq!(eval("<has file version->null-string<-has>"), "");
    assert_eq!(eval("<!has file version->null-string<-has>"), "null-string");
    assert_eq!(eval("<has year->null-int<-has>"), "");
    assert_eq!(eval("<has narrator->empty-list<-has>"), "");
    assert_eq!(eval("<is narrator[<1]->empty-list<-is>"), "empty-list");
}

#[test]
fn has_value() {
    assert_eq!(eval("<has id->true<-has>"), "true");
    assert_eq!(eval("<!has id->false<-has>"), "");
    assert_eq!(eval("<has title->true<-has>"), "true");
    assert_eq!(eval("<has author->true<-has>"), "true");
    assert_eq!(eval("<has series->true<-has>"), "true");
    assert_eq!(eval("<has series#->true<-has>"), "true");
    assert_eq!(eval("<has tag->true<-has>"), "true");
    assert_eq!(
        eval("<is title[=A Study in Scarlet: An Audible Original Drama]->true<-is>"),
        "true"
    );
    assert_eq!(eval("<is title[#=45]->true<-is>"), "true");
    assert_eq!(eval("<is title[!=foo]->true<-is>"), "true");
    assert_eq!(eval("<is title[~A Study.*]->true<-is>"), "true");
    assert_eq!(eval("<is author[>=2]->true<-is>"), "true");
    assert_eq!(eval("<is author[#=2]->true<-is>"), "true");
    assert_eq!(eval("<is author[=Arthur Conan Doyle]->true<-is>"), "true");
    assert_eq!(eval("<is author[format({L})][=Doyle]->true<-is>"), "true");
    assert_eq!(eval("<is tag[=Tag1]->true<-is>"), "true");
    assert_eq!(
        eval("<is tag[separator(:)slice(-2..)][=Tag2:Tag3]->true<-is>"),
        "true"
    );
    assert_eq!(eval("<is minutes[>42]->true<-is>"), "true");
}

#[test]
fn cmp() {
    assert_eq!(
        eval("<cmp title = 'A Study in Scarlet: An Audible Original Drama'->true<-cmp>"),
        "true"
    );
    assert_eq!(eval("<cmp title #= 45->true<-cmp>"), "true");
    assert_eq!(eval("<cmp 45 #= title->true<-cmp>"), "true");
    assert_eq!(eval("<cmp title != 'foo'->true<-cmp>"), "true");
    assert_eq!(eval("<cmp title ~ 'A Study.*'->true<-cmp>"), "true");
    assert_eq!(eval("<cmp author >= '3'->true<-cmp>"), "true");
    assert_eq!(
        eval("<cmp author = 'Arthur Conan Doyle'->true<-cmp>"),
        "true"
    );
    assert_eq!(
        eval("<cmp tag[separator(:)slice(-2..)] :contains: 'Tag2:Tag3'->true<-cmp>"),
        "true"
    );
    assert_eq!(eval("<cmp tag ≡ tag ->true<-cmp>"), "true");
    assert_eq!(eval("<cmp tag == tag ->true<-cmp>"), "true");
    assert_eq!(eval("<cmp tag :equals: tag ->true<-cmp>"), "true");
    assert_eq!(eval("<cmp tag ∋ tag[slice(2)] ->true<-cmp>"), "true");
    assert_eq!(eval("<cmp tag[slice(2)] ∈ tag ->true<-cmp>"), "true");
    assert_eq!(eval("<cmp tag[slice(1..2)] ⊆ tag ->true<-cmp>"), "true");
}

// ---------------------------------------------------------------------------
// if series / if abridged
// ---------------------------------------------------------------------------

#[test]
fn if_series() {
    // with series
    assert_eq!(
        eval("foo<if series->-<series>-<id>-<-if series>bar"),
        "foo-Sherlock Holmes-asin-bar"
    );
    // no series
    let mut b = book();
    b.series = vec![];
    assert_eq!(
        eval_book("foo<if series->-<series>-<id>-<-if series>bar", &b),
        "foobar"
    );
    // empty body
    assert_eq!(eval("foo<if series-><-if series>bar"), "foobar");
}

#[test]
fn if_abridged() {
    let mut b = book();
    b.is_abridged = true;
    assert_eq!(
        eval_book("<if abridged->Abridged<-if abridged>", &b),
        "Abridged"
    );
    b.is_abridged = false;
    assert_eq!(eval_book("<if abridged->Abridged<-if abridged>", &b), "");
}

// ---------------------------------------------------------------------------
// Legacy libation-rs syntax
// ---------------------------------------------------------------------------

#[test]
fn legacy_syntax() {
    assert_eq!(eval("%asin%"), "asin");
    assert_eq!(eval("<asin>"), "asin");
    assert_eq!(
        eval("<if series>-<series>-<id>-<end if>"),
        "-Sherlock Holmes-asin-"
    );
}

// ---------------------------------------------------------------------------
// Chapter context
// ---------------------------------------------------------------------------

#[test]
fn chapters() {
    let b = book();
    let ch = ChapterContext {
        chapter_number: 3,
        chapter_count: 12,
        chapter_title: Some("The Science of Deduction".into()),
        file_date: None,
    };
    assert_eq!(
        expand_template("<ch#> - <ch title>", &b, Some(&ch)).unwrap(),
        "3 - The Science of Deduction"
    );
    assert_eq!(expand_template("<ch# 0>", &b, Some(&ch)).unwrap(), "03");
    assert_eq!(expand_template("<ch count>", &b, Some(&ch)).unwrap(), "12");
}
