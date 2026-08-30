//! Turning stored paths into one browsable level (libraries design §4).
//!
//! Pure: `level_of` needs no database, and the edge cases are all about path
//! arithmetic — a file at the root, a folder name that repeats deeper down,
//! a path outside the level being asked for.

use alexandria_core::catalog::model::{File, FileState, FileType, FileView};
use alexandria_core::libraries::queries::level_of;
use chrono::Utc;
use uuid::Uuid;

/// A `FileView` for a file at `path`. Only the path matters here.
fn at(path: &str) -> (String, FileView) {
    (
        path.to_string(),
        FileView {
            // The tree arithmetic is about paths; membership is what put
            // these rows in front of it.
            library_uuid: None,
            file: File {
                uuid: Uuid::new_v4(),
                path: path.to_string(),
                name: path.rsplit('/').next().unwrap_or(path).to_string(),
                file_type: FileType::Document,
                content_hash: None,
                state: FileState::Active,
                deleted_at: None,
                indexed_at: Utc::now(),
                missing_at: None,
                size_bytes: None,
                mtime: None,
            },
            metadata: None,
            width: None,
            height: None,
            page_count: None,
            duration_seconds: None,
            comic_page_count: None,
        },
    )
}

fn names(folders: &[alexandria_core::libraries::model::LibraryFolder]) -> Vec<String> {
    folders.iter().map(|f| f.name.clone()).collect()
}

#[test]
fn given_the_top_level_when_listed_then_only_its_own_children_appear() {
    // The whole point of one level at a time: a course with two hundred
    // classes answers two hundred folders here, not every file in all of
    // them.
    let files = vec![
        at("class-01/lecture.mp4"),
        at("class-01/handout.pdf"),
        at("class-02/lecture.mp4"),
        at("syllabus.pdf"),
    ];

    let (folders, here) = level_of(files, "");

    assert_eq!(names(&folders), vec!["class-01", "class-02"]);
    assert_eq!(here.len(), 1, "only the file actually at the top");
    assert_eq!(here[0].file.name, "syllabus.pdf");
}

#[test]
fn given_many_files_in_one_folder_when_listed_then_it_is_named_once() {
    // Every file in a subfolder names that subfolder. Collected into a list
    // rather than a set, a class with twenty handouts would appear twenty
    // times.
    let files = vec![
        at("class-01/a.pdf"),
        at("class-01/b.pdf"),
        at("class-01/c.pdf"),
    ];

    let (folders, _) = level_of(files, "");

    assert_eq!(names(&folders), vec!["class-01"]);
}

#[test]
fn given_a_subfolder_when_listed_then_its_own_contents_come_back() {
    let files = vec![
        at("class-01/lecture.mp4"),
        at("class-01/handout.pdf"),
        at("class-02/lecture.mp4"),
    ];

    let (folders, here) = level_of(files, "class-01");

    assert!(folders.is_empty());
    assert_eq!(here.len(), 2);
    assert_eq!(
        here.iter().map(|v| v.file.name.clone()).collect::<Vec<_>>(),
        vec!["lecture.mp4", "handout.pdf"]
    );
}

#[test]
fn given_a_nested_folder_when_listed_then_deeper_files_are_not_included() {
    // One level. A file two folders down names the child it is under and
    // contributes nothing else to this answer.
    let files = vec![at("class-01/week-1/notes.pdf"), at("class-01/handout.pdf")];

    let (folders, here) = level_of(files, "class-01");

    assert_eq!(names(&folders), vec!["week-1"]);
    assert_eq!(here.len(), 1);
    assert_eq!(here[0].file.name, "handout.pdf");
}

#[test]
fn given_a_child_folder_when_listed_then_its_path_is_what_opens_it() {
    // The path a caller sends back to descend. Relative to the root, so it
    // is the same string on both platforms and survives the library moving.
    let files = vec![at("class-01/week-1/notes.pdf")];

    let (folders, _) = level_of(files, "class-01");

    assert_eq!(folders[0].path, "class-01/week-1");
    assert_eq!(folders[0].name, "week-1");
}

#[test]
fn given_a_repeated_folder_name_when_listed_then_the_deeper_one_is_not_confused() {
    // `notes` exists at the top and inside a class. Matching on the name
    // rather than the full relative path would merge them.
    let files = vec![at("notes/a.pdf"), at("class-01/notes/b.pdf")];

    let (top, _) = level_of(files.clone(), "");
    let (inside, _) = level_of(files, "class-01");

    assert_eq!(names(&top), vec!["class-01", "notes"]);
    assert_eq!(inside[0].path, "class-01/notes");
}

#[test]
fn given_a_folder_with_nothing_in_it_when_listed_then_it_is_simply_empty() {
    let (folders, here) = level_of(vec![at("class-01/a.pdf")], "class-02");

    assert!(folders.is_empty());
    assert!(here.is_empty());
}

#[test]
fn given_a_prefix_that_is_not_a_folder_boundary_when_listed_then_it_does_not_match() {
    // `class-01` must not swallow `class-011`: a prefix comparison without
    // the separator would put another class's files inside this one.
    let files = vec![at("class-011/a.pdf"), at("class-01/b.pdf")];

    let (_, here) = level_of(files, "class-01");

    assert_eq!(here.len(), 1);
    assert_eq!(here[0].file.name, "b.pdf");
}
