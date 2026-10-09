//! Whole imports, from fixture folders built in temp dirs.

use super::*;
use crate::model::{find_block, page_aliases};
use crate::storage::Storage;
use uuid::Uuid;

const STAMP: &str = "2026-10-09 14.03";

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("notesec-import-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A folder with `files` (relative path, contents).
fn fixture(name: &str, files: &[(&str, &[u8])]) -> PathBuf {
    let dir = temp(name);
    for (rel, bytes) in files {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    dir
}

/// An empty graph (as `Storage::open` makes it), with `pages`.
fn graph(name: &str, pages: &[(&str, &str)]) -> (PathBuf, Existing) {
    let dir = temp(&format!("{name}-graph"));
    let _ = Storage::open(dir.clone()).unwrap();
    fs::create_dir_all(dir.join("assets")).unwrap();
    for (title, md) in pages {
        fs::write(crate::storage::page_file(&dir, title, false), md).unwrap();
    }
    let existing = existing_of(&dir);
    (dir, existing)
}

/// What the app would pass for the graph at `dir`.
fn existing_of(dir: &Path) -> Existing {
    let loaded = Storage::open(dir.to_path_buf()).unwrap().load_all();
    Existing {
        titles: loaded.iter().map(|p| p.title.clone()).collect(),
        aliases: loaded.iter().flat_map(page_aliases).collect(),
        ids: loaded
            .iter()
            .flat_map(|p| p.saved_ids.iter().copied())
            .collect(),
    }
}

/// Every `id::` line in the graph's pages and journals.
fn ids_on_disk(graph: &Path) -> Vec<Uuid> {
    let mut ids = Vec::new();
    for sub in ["pages", "journals"] {
        for entry in fs::read_dir(graph.join(sub)).unwrap().flatten() {
            let text = fs::read_to_string(entry.path()).unwrap();
            ids.extend(
                text.lines()
                    .filter_map(|l| Uuid::parse_str(l.trim().strip_prefix("id::")?.trim()).ok()),
            );
        }
    }
    ids
}

fn page_text(graph: &Path, title: &str, journal: bool) -> String {
    let path = crate::storage::page_file(graph, title, journal);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn run(
    source: Source,
    folder: &Path,
    graph: &Path,
    existing: &Existing,
    on_clash: OnClash,
) -> Summary {
    let plan = plan(source, folder, graph, existing).unwrap();
    apply(plan, on_clash, graph, existing, STAMP).unwrap()
}

#[test]
fn an_obsidian_vault_becomes_pages_with_links_and_assets() {
    let vault = fixture(
        "obsidian",
        &[
            (
                "Home.md",
                b"---\ntags: [project, \"big idea\"]\naliases: [Start]\nstatus: draft\n---\n# Home\nSee [[Notes/Ideas|my ideas]] and [[Ideas#Part]] and [[Missing]].\n\n![[Ideas]]\n![[pic.png|300]]\n[doc](files/My%20Doc.pdf)\n[[#Section]] and `[[code]]`\n- [ ] todo\n",
            ),
            ("Notes/Ideas.md", b"notes ideas\n"),
            ("Archive/Ideas.md", b"archived ideas\n"),
            ("2026-10-09.md", b"daily\n"),
            ("attachments/pic.png", b"png bytes"),
            ("files/My Doc.pdf", b"pdf bytes"),
            ("unused.png", b"unused"),
            (".obsidian/workspace.json", b"{}"),
            (".trash/Old.md", b"old"),
        ],
    );
    let (graph, existing) = graph("obsidian", &[]);
    fs::write(graph.join("assets/pic.png"), b"mine").unwrap();
    let plan = plan(Source::Obsidian, &vault, &graph, &existing).unwrap();
    assert!(plan.clashes.is_empty());
    let titles: Vec<&str> = plan.drafts.iter().map(|d| d.title.as_str()).collect();
    // Nearest the root first; the second "Ideas" gets its folder.
    assert_eq!(titles, ["2026-10-09", "Home", "Ideas", "Notes/Ideas"]);
    let summary = apply(plan, OnClash::Rename, &graph, &existing, STAMP).unwrap();
    assert_eq!(summary.pages.len(), 4);
    // `[[code]]` in backticks too: the app reads it as a link as well.
    assert_eq!(
        (summary.assets, summary.unresolved, summary.renamed),
        (2, 2, 0)
    );
    assert_eq!(summary.log_title, "Import from Obsidian 2026-10-09 14.03");
    assert_eq!(
        summary.text(),
        "Imported 4 pages and 2 assets from Obsidian, 2 unresolved links (see \u{201c}Import from Obsidian 2026-10-09 14.03\u{201d})"
    );

    let home = page_text(&graph, "Home", false);
    assert_eq!(
        home,
        "- tags:: #project, #[[big idea]]\n  alias:: Start\n  status:: draft\n  imported-from:: [[Import from Obsidian 2026-10-09 14.03]]\n\
         - # Home\n  - See [[Notes/Ideas]] and [[Ideas]] and [[Missing]].\n  - ![[Ideas]]\n    ![pic.png](../assets/pic-2.png)\n    [doc](../assets/My_Doc.pdf)\n    Section and `[[code]]`\n  - TODO todo\n"
    );
    // The page properties work: the alias resolves.
    let page = crate::model::Page::from_markdown("Home", false, &home);
    assert_eq!(page_aliases(&page), ["Start"]);
    assert!(page_text(&graph, "Ideas", false).contains("archived ideas"));
    assert!(page_text(&graph, "2026-10-09", true).contains("daily"));
    assert_eq!(
        fs::read(graph.join("assets/pic.png")).unwrap(),
        b"mine",
        "never overwritten"
    );
    assert_eq!(
        fs::read(graph.join("assets/pic-2.png")).unwrap(),
        b"png bytes"
    );
    assert!(graph.join("assets/My_Doc.pdf").exists());
    assert!(!graph.join("assets/unused.png").exists());
    let log = page_text(&graph, &summary.log_title, false);
    assert!(log.contains("  - [[Home]]\n") && log.contains("`Missing`") && log.contains("To undo"));
    // Everything loads as pages.
    let pages = Storage::open(graph.clone()).unwrap().load_all();
    assert_eq!(pages.len(), 5);
    let _ = fs::remove_dir_all(vault);
    let _ = fs::remove_dir_all(graph);
}

#[test]
fn taken_names_are_renamed_or_skipped_and_links_follow() {
    let vault = fixture(
        "clash",
        &[
            ("Home.md", b"mine [[Other]]\n"),
            ("Other.md", b"back to [[Home]] and #Home\n"),
            ("2026-10-09.md", b"imported day\n"),
        ],
    );
    let pages = [("Home", "- the graph's own home\n")];
    let (g, existing) = graph("clash-rename", &pages);
    fs::write(g.join("journals/2026_10_09.md"), "- my day\n").unwrap();
    let existing = Existing {
        titles: [existing.titles, vec!["2026-10-09".into()]].concat(),
        ..existing
    };
    let p = plan(Source::Obsidian, &vault, &g, &existing).unwrap();
    assert_eq!(p.clashes, ["2026-10-09", "Home"]);
    let summary = apply(p, OnClash::Rename, &g, &existing, STAMP).unwrap();
    assert_eq!(summary.renamed, 2);
    assert_eq!(
        fs::read_to_string(g.join("pages/Home.md")).unwrap(),
        "- the graph's own home\n"
    );
    assert_eq!(
        fs::read_to_string(g.join("journals/2026_10_09.md")).unwrap(),
        "- my day\n"
    );
    assert!(page_text(&g, "Home (imported)", false).contains("mine [[Other]]"));
    assert!(page_text(&g, "Other", false)
        .contains("back to [[Home (imported)]] and #[[Home (imported)]]"));
    // A date that is taken becomes a normal page.
    assert!(page_text(&g, "2026-10-09 (imported)", false).contains("imported day"));

    let (g2, existing2) = graph("clash-skip", &pages);
    let summary = run(Source::Obsidian, &vault, &g2, &existing2, OnClash::Skip);
    assert_eq!(
        (summary.pages.len(), summary.skipped, summary.unresolved),
        (2, 1, 0)
    );
    assert_eq!(
        fs::read_to_string(g2.join("pages/Home.md")).unwrap(),
        "- the graph's own home\n"
    );
    assert!(page_text(&g2, "Other", false).contains("back to [[Home]]"));
    assert!(page_text(&g2, &summary.log_title, false)
        .contains("`Home.md`: \u{201c}Home\u{201d} already exists"));

    // A file already on disk (not loaded) is never overwritten either.
    let (g3, existing3) = graph("clash-file", &[]);
    fs::write(g3.join("pages/Other.md"), "- on disk\n").unwrap();
    let summary = run(Source::Obsidian, &vault, &g3, &existing3, OnClash::Rename);
    assert_eq!(
        fs::read_to_string(g3.join("pages/Other.md")).unwrap(),
        "- on disk\n"
    );
    assert_eq!(summary.skipped, 1);
    for dir in [vault, g, g2, g3] {
        let _ = fs::remove_dir_all(dir);
    }
}

#[test]
fn a_logseq_graph_keeps_block_refs_journals_and_properties() {
    let id = Uuid::new_v4();
    let alpha = format!(
        "title:: Project/Alpha\ntags:: work, [[big deal]]\nalias:: PA\n\n- plan\n  id:: {id}\n  - child\n- see [[Oct 9th, 2026]] and ![](../assets/img.png)\n"
    );
    let other = format!("- ref (({id}))\n- #[[Project/Alpha]] [[pa]]\n");
    let vault = fixture(
        "logseq",
        &[
            ("pages/Project___Alpha.md", alpha.as_bytes()),
            ("pages/Other%2FPage.md", other.as_bytes()),
            ("journals/2026_10_09.md", b"- journal entry\n"),
            ("assets/img.png", b"img"),
            (
                "logseq/config.edn",
                b"{:journal/page-title-format \"MMM do, yyyy\"}",
            ),
            ("logseq/bak/pages/Project___Alpha.md", b"- backup\n"),
            ("pages/Notes.org", b"* org\n"),
            ("whiteboards/board.edn", b"{}"),
        ],
    );
    let (graph, existing) = graph("logseq", &[]);
    fs::write(graph.join("assets/img.png"), b"mine").unwrap();
    let summary = run(Source::Logseq, &vault, &graph, &existing, OnClash::Rename);
    assert_eq!(
        (summary.pages.len(), summary.assets, summary.unresolved),
        (3, 1, 0)
    );
    assert_eq!(summary.skipped, 2, "the .org file and the whiteboard");
    let alpha = page_text(&graph, "Project/Alpha", false);
    assert!(
        alpha.starts_with("- tags:: #work, #[[big deal]]\n  alias:: PA\n  imported-from::"),
        "{alpha}"
    );
    assert!(!alpha.contains("title::"));
    assert!(alpha.contains(&format!("- plan\n  id:: {id}\n  - child\n")));
    assert!(alpha.contains("- see [[2026-10-09]] and ![](../assets/img-2.png)"));
    assert!(page_text(&graph, "Other/Page", false).contains(&format!("ref (({id}))")));
    assert!(page_text(&graph, "2026-10-09", true).contains("journal entry"));
    // The reference resolves after loading, and the backup wasn't imported.
    let pages = Storage::open(graph.clone()).unwrap().load_all();
    let (p, _) = find_block(&pages, id).unwrap();
    assert_eq!(pages[p].title, "Project/Alpha");
    assert!(!pages
        .iter()
        .any(|p| p.blocks.iter().any(|b| b.content == "backup")));

    let not_logseq = fixture("logseq-not", &[("a.md", b"x")]);
    let err = plan(Source::Logseq, &not_logseq, &graph, &existing).unwrap_err();
    assert!(err.contains("is it a Logseq graph?"), "{err}");
    for dir in [vault, graph, not_logseq] {
        let _ = fs::remove_dir_all(dir);
    }
}

#[test]
fn importing_a_logseq_graph_twice_never_duplicates_block_ids() {
    let x = Uuid::new_v4();
    let vault = fixture(
        "logseq-twice",
        &[
            ("pages/Alpha.md", format!("- plan\n  id:: {x}\n").as_bytes()),
            // The same id again in the export: the first block keeps it.
            ("pages/Dup.md", format!("- copy\n  id:: {x}\n").as_bytes()),
            (
                "pages/Other.md",
                format!("- ref (({x}))\n- embed ![[(({x}))]]\n").as_bytes(),
            ),
        ],
    );
    let (graph, existing) = graph("logseq-twice", &[]);
    let first = run(Source::Logseq, &vault, &graph, &existing, OnClash::Rename);
    assert_eq!((first.pages.len(), first.new_ids), (3, 1));
    let pages = Storage::open(graph.clone()).unwrap().load_all();
    let (p, _) = find_block(&pages, x).unwrap();
    assert_eq!(pages[p].title, "Alpha");
    assert!(page_text(&graph, "Other", false).contains(&format!("- ref (({x}))")));
    assert!(page_text(&graph, &first.log_title, false).contains(
        "Block ids renewed, already in this graph or repeated in the import: 1 (0 remapped"
    ));

    // A page of the graph's own pointing at the first import's block.
    let mine = format!("- mine (({x}))\n");
    fs::write(graph.join("pages/Mine.md"), &mine).unwrap();
    let second = run(
        Source::Logseq,
        &vault,
        &graph,
        &existing_of(&graph),
        OnClash::Rename,
    );
    assert_eq!(
        (second.pages.len(), second.renamed, second.new_ids),
        (3, 3, 2)
    );
    let pages = Storage::open(graph.clone()).unwrap().load_all();
    // The graph's references still reach the original.
    assert_eq!(
        fs::read_to_string(graph.join("pages/Mine.md")).unwrap(),
        mine
    );
    let (p, _) = find_block(&pages, x).unwrap();
    assert_eq!(pages[p].title, "Alpha");
    // The second import's references reach its own copy.
    let copy = pages
        .iter()
        .find(|p| p.title == "Alpha (imported)")
        .unwrap()
        .blocks
        .iter()
        .find(|b| b.content == "plan")
        .unwrap()
        .id;
    assert_ne!(copy, x);
    let other = page_text(&graph, "Other (imported)", false);
    assert!(other.contains(&format!("- ref (({copy}))")), "{other}");
    assert!(
        other.contains(&format!("- embed ![[(({copy}))]]")),
        "{other}"
    );
    assert!(!other.contains(&x.to_string()), "{other}");
    assert!(page_text(&graph, &second.log_title, false).contains(": 2 (1 remapped"));
    // No id twice anywhere in the graph.
    let ids = ids_on_disk(&graph);
    let unique: HashSet<Uuid> = ids.iter().copied().collect();
    assert_eq!(ids.len(), 4, "{ids:?}");
    assert_eq!(unique.len(), ids.len(), "{ids:?}");
    for dir in [vault, graph] {
        let _ = fs::remove_dir_all(dir);
    }
}

#[test]
fn ids_of_trashed_pages_count_as_taken() {
    let x = Uuid::new_v4();
    let vault = fixture(
        "logseq-trash",
        &[(
            "pages/Alpha.md",
            format!("- plan\n  id:: {x}\n- see (({x}))\n").as_bytes(),
        )],
    );
    let (graph, existing) = graph("logseq-trash", &[]);
    // A deleted page can be restored, so its ids stay reserved.
    fs::create_dir_all(graph.join(".trash/1700000000000/pages")).unwrap();
    fs::write(
        graph.join(".trash/1700000000000/pages/Old.md"),
        format!("- old\n  id:: {x}\n"),
    )
    .unwrap();
    let summary = run(Source::Logseq, &vault, &graph, &existing, OnClash::Rename);
    assert_eq!(summary.new_ids, 1);
    let alpha = page_text(&graph, "Alpha", false);
    assert!(!alpha.contains(&x.to_string()), "{alpha}");
    let pages = Storage::open(graph.clone()).unwrap().load_all();
    let id = pages
        .iter()
        .find(|p| p.title == "Alpha")
        .unwrap()
        .blocks
        .iter()
        .find(|b| b.content == "plan")
        .unwrap()
        .id;
    assert!(alpha.contains(&format!("- see (({id}))")), "{alpha}");
    for dir in [vault, graph] {
        let _ = fs::remove_dir_all(dir);
    }
}

#[test]
fn a_notion_export_loses_its_ids_and_keeps_its_links_and_databases() {
    let id = |n: u8| format!("{n:x}").repeat(32);
    let (h, c, t, w, o, oc) = (id(1), id(2), id(3), id(4), id(5), id(6));
    let home = format!(
        "# Home\n\nSee [Child](Home%20{h}/Child%20{c}.md) and [Tasks](Home%20{h}/Tasks%20{t}.csv).\n![shot](Home%20{h}/shot.png)\n[web](https://www.notion.so/Child-{c})\n"
    );
    let csv = "\u{feff}Name,Status,Tags\nWrite report,Done,\"work, big deal\"\nPlain row,\"multi\nline\",x|y\n";
    let files: Vec<(String, Vec<u8>)> = vec![
        (format!("Home {h}.md"), home.into_bytes()),
        (
            format!("Home {h}/Child {c}.md"),
            b"# Child\n\nchild text\n".to_vec(),
        ),
        (
            format!("Home {h}/Tasks {t}.csv"),
            b"Name\nonly some rows\n".to_vec(),
        ),
        (
            format!("Home {h}/Tasks {t}_all.csv"),
            csv.as_bytes().to_vec(),
        ),
        (
            format!("Home {h}/Tasks {t}/Write report {w}.md"),
            b"# Write report\n\nStatus: Done\nTags: work, big deal\n\nbody\n".to_vec(),
        ),
        (format!("Home {h}/shot.png"), b"shot".to_vec()),
        (
            format!("Other {o}/Child {oc}.md"),
            b"# Child\n\nanother child\n".to_vec(),
        ),
    ];
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_slice()))
        .collect();
    let export = fixture("notion", &refs);
    let (graph, existing) = graph("notion", &[]);
    let summary = run(Source::Notion, &export, &graph, &existing, OnClash::Rename);
    assert_eq!(
        (summary.pages.len(), summary.assets, summary.unresolved),
        (5, 1, 0)
    );
    let home = page_text(&graph, "Home", false);
    assert!(!home.contains("# Home"), "the title line is dropped");
    assert!(
        home.contains("See [[Child]] and [[Tasks]].\n  ![shot](../assets/shot.png)\n  [[Child]]"),
        "{home}"
    );
    assert_eq!(
        page_text(&graph, "Tasks", false)
            .lines()
            .skip(1)
            .collect::<Vec<_>>(),
        [
            "- | Name | Status | Tags |",
            "  | --- | --- | --- |",
            "  | [[Write report]] | Done | work, big deal |",
            "  | Plain row | multi line | x\\|y |",
        ]
    );
    let row = page_text(&graph, "Write report", false);
    assert!(
        row.starts_with("- tags:: #work, #[[big deal]]\n  status:: Done\n  imported-from::"),
        "{row}"
    );
    assert!(row.contains("- body"));
    assert!(page_text(&graph, "Other/Child", false).contains("another child"));

    let zip = export.join("export.zip");
    fs::write(&zip, b"PK").unwrap();
    let err = plan(Source::Notion, &zip, &graph, &existing).unwrap_err();
    assert!(err.contains("unzip"), "{err}");
    for dir in [export, graph] {
        let _ = fs::remove_dir_all(dir);
    }
}

#[test]
fn caps_refusals_and_odd_names() {
    let big = vec![b'x'; usize::try_from(MAX_NOTE_BYTES).unwrap() + 1];
    let vault = fixture(
        "caps",
        &[
            ("Big.md", big.as_slice()),
            ("Bad.md", b"\xff\xfe not utf-8"),
            ("Ok.md", b"fine"),
            ("A___B.md", b"triple"),
        ],
    );
    let (graph, existing) = graph("caps", &[]);
    let summary = run(Source::Obsidian, &vault, &graph, &existing, OnClash::Rename);
    assert_eq!((summary.pages.len(), summary.skipped), (2, 2));
    assert!(page_text(&graph, "A_B", false).contains("triple"));
    let log = page_text(&graph, &summary.log_title, false);
    assert!(log.contains("`Big.md`: larger than 5 MB") && log.contains("`Bad.md`: not UTF-8 text"));
    // Importing a graph into itself (or a folder inside it) is refused.
    let err = plan(Source::Logseq, &graph, &graph, &existing).unwrap_err();
    assert!(err.contains("outside this graph"), "{err}");
    let err = plan(Source::Obsidian, &graph.join("pages"), &graph, &existing).unwrap_err();
    assert!(err.contains("outside this graph"), "{err}");
    // Nothing to import.
    let empty = temp("caps-empty");
    assert!(plan(Source::Obsidian, &empty, &graph, &existing)
        .unwrap_err()
        .starts_with("No Obsidian notes"));
    // A second import gets its own log page.
    let again = run(
        Source::Obsidian,
        &vault,
        &graph,
        &Existing {
            titles: Storage::open(graph.clone())
                .unwrap()
                .load_all()
                .iter()
                .map(|p| p.title.clone())
                .collect(),
            ..Default::default()
        },
        OnClash::Skip,
    );
    assert_eq!(again.log_title, "Import from Obsidian 2026-10-09 14.03 (2)");
    assert_eq!(again.pages.len(), 0);
    assert_eq!(clean_title(&"é".repeat(100)).len(), 150);
    assert_eq!(clean_title(" \t\n"), "Untitled");
    for dir in [vault, graph, empty] {
        let _ = fs::remove_dir_all(dir);
    }
}
