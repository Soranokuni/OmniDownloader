//! Taxonomy in the database (plan P4.17): groups, membership, and the
//! taxonomy.json import/export. All names and addresses are invented.

use omni_core::repository::Repository;
use omni_core::taxonomy::{Group, Taxonomy};

fn repo() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::new(dir.path().join("omni.db")).unwrap();
    (dir, repo)
}

fn file() -> Taxonomy {
    serde_json::from_str(
        r#"{
          "version": 1,
          "groups": [
            {"code": "NEWS", "name": "Κεντρικό Δελτίο", "kind": "news", "keywords": ["ΔΕΛΤΙΟ"],
             "description": "Ειδήσεις της ημέρας"},
            {"code": "KALIMERA", "name": "Καλημέρα Κρήτη", "keywords": ["ΚΑΛΗΜΕΡΑ ΚΡΗΤΗ"]},
            {"code": "SPORTS", "name": "Αθλητικά", "kind": "desk"}
          ],
          "people": [
            {"surname": "PAPADAKI", "full_name": "Anna Papadaki", "emails": ["a.papadaki@example.gr"],
             "aliases": ["ΠΑΠΑΔΑΚΗ"], "default_priority": 5, "groups": ["NEWS", "KALIMERA"]},
            {"surname": "NIKOLAOU", "full_name": "Giorgos Nikolaou", "emails": ["g.nikolaou@example.gr"],
             "groups": ["SPORTS"]}
          ]
        }"#,
    )
    .unwrap()
}

#[test]
fn an_imported_file_exports_back_the_same() {
    let (_d, repo) = repo();
    let r = repo.import_taxonomy(&file(), false).unwrap();
    assert_eq!((r.groups_saved, r.people_saved), (3, 2));

    let out = repo.export_taxonomy().unwrap();
    assert_eq!(out.groups.len(), 3);
    let papadaki = out.people.iter().find(|p| p.surname == "PAPADAKI").unwrap();
    assert_eq!(papadaki.groups, vec!["NEWS", "KALIMERA"], "order is the default first");
    assert_eq!(papadaki.aliases, vec!["ΠΑΠΑΔΑΚΗ"]);
    assert_eq!(papadaki.default_priority, 5);
    assert!(out.people.iter().all(|p| p.surname != "MCR"), "MCR is built in, not exported");

    // The roster the parser reads carries the groups too.
    let j = repo.list_journalists().unwrap().into_iter().find(|j| j.surname == "NIKOLAOU").unwrap();
    assert_eq!(j.groups, vec!["SPORTS"]);
}

#[test]
fn merge_keeps_what_the_file_does_not_mention_and_replace_removes_it() {
    let (_d, repo) = repo();
    repo.import_taxonomy(&file(), false).unwrap();
    repo.save_journalist("GEORGIOU", "Eleni Georgiou", &[], 0).unwrap();

    let mut smaller = file();
    smaller.groups.retain(|g| g.code != "SPORTS");
    smaller.people.retain(|p| p.surname == "PAPADAKI");

    repo.import_taxonomy(&smaller, false).unwrap();
    assert_eq!(repo.list_groups().unwrap().len(), 3, "merge deleted a group");
    assert_eq!(repo.list_journalists().unwrap().len(), 4, "merge deleted a person (MCR + 3)");

    let r = repo.import_taxonomy(&smaller, true).unwrap();
    assert_eq!((r.groups_removed, r.people_removed), (1, 2), "{r:?}");
    let surnames: Vec<String> = repo.list_journalists().unwrap().into_iter().map(|j| j.surname).collect();
    assert_eq!(surnames, vec!["MCR", "PAPADAKI"]);
}

#[test]
fn a_bad_file_changes_nothing_and_names_every_problem() {
    let (_d, repo) = repo();
    repo.import_taxonomy(&file(), false).unwrap();

    let mut bad = file();
    bad.groups[0].name = "Άλλο όνομα".into();
    bad.people[1].groups = vec!["WEATHER".into()];
    bad.people[1].emails = vec!["a.papadaki@example.gr".into()];
    let err = repo.import_taxonomy(&bad, false).unwrap_err().to_string();
    assert!(err.contains("no group WEATHER") && err.contains("belongs to two people"), "{err}");

    let news = repo.list_groups().unwrap().into_iter().find(|g| g.code == "NEWS").unwrap();
    assert_eq!(news.name, "Κεντρικό Δελτίο", "a refused import still wrote");

    let mut mcr = file();
    mcr.people[0].surname = "MCR".into();
    assert!(repo.import_taxonomy(&mcr, false).is_err());
}

#[test]
fn deleting_a_group_drops_its_memberships_and_only_those() {
    let (_d, repo) = repo();
    repo.import_taxonomy(&file(), false).unwrap();
    assert!(repo.delete_group("news").unwrap());
    let j = repo.list_journalists().unwrap().into_iter().find(|j| j.surname == "PAPADAKI").unwrap();
    assert_eq!(j.groups, vec!["KALIMERA"], "the next group becomes the default");
    assert!(!repo.delete_group("NEWS").unwrap());
}

#[test]
fn groups_are_validated_when_saved_one_by_one_too() {
    let (_d, repo) = repo();
    let saved = repo
        .save_group(&Group {
            code: " weather ".into(),
            name: "Καιρός".into(),
            kind: "Desk".into(),
            keywords: vec!["ΚΑΙΡΟΣ".into()],
            description: String::new(),
        })
        .unwrap();
    assert_eq!((saved.code.as_str(), saved.kind.as_str()), ("WEATHER", "desk"));
    let bad = Group { code: "Χ".into(), ..saved.clone() };
    assert!(repo.save_group(&bad).is_err());

    repo.save_journalist("PAPADAKI", "Anna Papadaki", &[], 0).unwrap();
    assert!(repo.set_journalist_groups("PAPADAKI", &["NOPE".into()]).is_err());
    repo.set_journalist_groups("papadaki", &["WEATHER".into()]).unwrap();
}

#[test]
fn a_taxonomy_json_next_to_the_database_seeds_the_first_start() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("taxonomy.json"), serde_json::to_string(&file()).unwrap()).unwrap();
    let repo = Repository::new(dir.path().join("omni.db")).unwrap();
    assert_eq!(repo.list_groups().unwrap().len(), 3);

    // Once groups exist the file is not read again: the panel is the truth.
    std::fs::write(dir.path().join("taxonomy.json"), r#"{"version":1,"groups":[]}"#).unwrap();
    repo.delete_group("SPORTS").unwrap();
    let again = Repository::new(dir.path().join("omni.db")).unwrap();
    assert_eq!(again.list_groups().unwrap().len(), 2);
}

#[test]
fn the_shipped_example_file_imports_cleanly() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/taxonomy.example.json");
    let t: Taxonomy = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let (_d, repo) = repo();
    let r = repo.import_taxonomy(&t, false).unwrap();
    assert!(r.groups_saved >= 3 && r.people_saved >= 2, "{r:?}");
}
