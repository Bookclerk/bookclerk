//! Tests for adding a copy to a stored book.

use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use super::super::tests::{
    edition_book, edition_req, job_id, media_in_process, mp3_frame_shape, output_copy, short_id,
    test_store, title_named_destinations, walk, RealAudioSource,
};
use super::*;
use crate::pipeline::acquire_book;

/// Stores `product_id` as an M4B through a whole acquire into `root`, and
/// returns the request that names it, the destinations and the main key.
async fn stored_book(
    store: &LibraryStore,
    cache: &Path,
    root: &Path,
    product_id: &str,
) -> (AcquireRequest, AcquireDestinations, String) {
    let mut destinations = title_named_destinations(root, Arc::new(AtomicUsize::new(0)));
    for item in &mut destinations.items {
        item.options.format = bookclerk_config::OutputFormat::EnrichedM4b;
    }
    let source = RealAudioSource {
        fetches: Arc::new(AtomicUsize::new(0)),
        pdf_url: None,
        mp3: false,
        chapters: Vec::new(),
    };
    let job = job_id(store).await;
    let mut req = edition_req(cache, job, product_id, "user-1");
    req.options = destinations.primary_destination().options.clone();
    let result = acquire_book(store, &destinations, req.clone(), &source)
        .await
        .expect("the book is stored");
    (req, destinations, result.storage_key)
}

/// A convert adds the copy beside the original, from the stored M4B,
/// under a folder named after the copy, records it as that copy, and
/// leaves the title's key alone; a second convert finds it.
#[tokio::test]
async fn a_convert_adds_the_copy_beside_the_original() {
    media_in_process();
    let store = test_store().await;
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("cache");
    tokio::fs::create_dir_all(&cache).await.unwrap();
    let root = tmp.path().join("local");
    let uuid = edition_book(&store, "B00CONV", "user-1", "Twin", None).await;
    let id = short_id(&store, &uuid).await;
    let (req, destinations, main_key) = stored_book(&store, &cache, &root, "B00CONV").await;
    assert!(main_key.ends_with(".m4b"), "{main_key}");

    let copy = output_copy("mp3:64k-mono");
    let result = convert_copy(&store, &destinations, req.clone(), &copy)
        .await
        .expect("the copy is made");
    assert!(!result.matched_existing);
    assert_eq!(result.store_fetches, 0, "no store is asked");
    let folder = format!("mp3 64k-mono/Twin [{id}]/");
    assert!(
        result.storage_key.starts_with(&folder) && result.storage_key.ends_with(".mp3"),
        "{}",
        result.storage_key
    );
    let bytes = std::fs::read(root.join(&result.storage_key)).unwrap();
    assert_eq!(mp3_frame_shape(&bytes), Some((64, true)));
    assert_eq!(
        store
            .edition_file_copy("local", &result.storage_key)
            .await
            .unwrap(),
        Some(copy.clone())
    );
    assert_eq!(
        store.edition_file_copy("local", &main_key).await.unwrap(),
        Some(output_copy("m4b:original")),
        "the original stays"
    );
    let book = store.get_book_by_uuid(&uuid).await.unwrap().unwrap();
    assert_eq!(
        book.storage_key.as_deref(),
        Some(main_key.as_str()),
        "the title's key is still the original's"
    );
    assert!(root.join(&main_key).exists());
    let pdfs = walk(&root)
        .into_iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "pdf"))
        .count();
    assert_eq!(pdfs, 0);
    let scratch: Vec<_> = std::fs::read_dir(cache.join("acquire"))
        .map(|dir| dir.map(|entry| entry.unwrap().path()).collect())
        .unwrap_or_default();
    assert!(scratch.is_empty(), "scratch was cleaned: {scratch:?}");

    let again = convert_copy(&store, &destinations, req.clone(), &copy)
        .await
        .unwrap();
    assert!(again.matched_existing, "the copy exists already");
    assert_eq!(again.storage_key, result.storage_key);

    let mut forced = req;
    forced.force = true;
    let remade = convert_copy(&store, &destinations, forced, &copy)
        .await
        .unwrap();
    assert!(!remade.matched_existing, "force makes it again");
}

/// A convert refuses what is not a new copy, and a book not stored.
#[tokio::test]
async fn a_convert_refuses_what_it_cannot_make() {
    media_in_process();
    let store = test_store().await;
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("cache");
    tokio::fs::create_dir_all(&cache).await.unwrap();
    let root = tmp.path().join("local");
    edition_book(&store, "B00CONV2", "user-1", "Twin", None).await;
    let (req, destinations, _) = stored_book(&store, &cache, &root, "B00CONV2").await;

    for (copy, said) in [
        ("original:original", "new encoding"),
        ("m4b:original", "main output"),
    ] {
        let err = convert_copy(&store, &destinations, req.clone(), &output_copy(copy))
            .await
            .unwrap_err();
        assert!(err.to_string().contains(said), "{copy}: {err}");
    }

    edition_book(&store, "B00NOTYET", "user-1", "Other", None).await;
    let job = req.job_id.clone().unwrap();
    let mut pending = edition_req(&cache, job, "B00NOTYET", "user-1");
    pending.options = destinations.primary_destination().options.clone();
    let err = convert_copy(&store, &destinations, pending, &output_copy("mp3:64k"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not stored yet"), "{err}");
}

/// The source is a copy kept as delivered or as an M4B before an encoded
/// one, the main output's before another, and the primary destination's.
#[test]
fn the_least_encoded_copy_is_the_source() {
    let file = |dest: &str, key: &str, copy: Option<&str>| ProbedAudioFile {
        destination: dest.into(),
        storage_key: key.into(),
        role: FileRole::Audio,
        copy: copy.map(output_copy),
        probe: None,
        stream: bookclerk_library::AudioStream::default(),
    };
    let tmp = tempfile::tempdir().unwrap();
    let mut destinations = title_named_destinations(tmp.path(), Arc::new(AtomicUsize::new(0)));
    let mut phone = title_named_destinations(tmp.path(), Arc::new(AtomicUsize::new(0)))
        .items
        .remove(0);
    phone.id = bookclerk_config::DestinationId::new("phone").unwrap();
    destinations.items.push(phone);
    let main = output_copy("mp3:128k");
    let wanted = output_copy("opus:32k");
    let files = vec![
        file("phone", "p/a.m4b", Some("m4b:original")),
        file("local", "l/a.mp3", Some("mp3:128k")),
        file("local", "l/b.mp3", Some("mp3:64k")),
        file("local", "l/c.opus", Some("opus:32k")),
        file("gone", "g/a.m4b", Some("original:original")),
    ];
    let (dest, chosen) = source_copy(&files, &destinations, Some(&main), &wanted).unwrap();
    assert_eq!(dest.id.as_str(), "phone", "the M4B wins over the MP3s");
    assert_eq!(chosen.len(), 1);
    let (dest, chosen) = source_copy(&files[1..], &destinations, Some(&main), &wanted).unwrap();
    assert_eq!(
        (dest.id.as_str(), chosen[0].storage_key.as_str()),
        ("local", "l/a.mp3"),
        "then the main output"
    );
    assert!(
        source_copy(&files[3..], &destinations, Some(&main), &wanted).is_none(),
        "the wanted copy and a destination not here are never the source"
    );
}

/// Extensions come from the key's file name, letters and digits only.
#[test]
fn scratch_extensions_are_tame() {
    assert_eq!(extension_of("a/b/Book.M4B"), "m4b");
    assert_eq!(extension_of("a/b.dir/Book"), "bin");
    assert_eq!(extension_of("Book./../x"), "bin");
    assert_eq!(extension_of("Book.mp3?"), "mp3");
}
