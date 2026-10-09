use super::protocol::*;
use super::sandbox::*;
use super::*;

const EXAMPLE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/plugins/word-count");

fn wasm(wat_src: &str) -> Vec<u8> {
    wat::parse_str(wat_src).unwrap()
}

/// A plugin whose `run_command` body is `body` (returns an i64).
fn plugin(body: &str) -> Compiled {
    compile(&wasm(&format!(
        r#"(module
             (memory (export "memory") 1)
             (data (i32.const 100) "[[actions]]\ntype = \"set_status\"\ntext = \"hi\"\n")
             (func (export "alloc") (param i32) (result i32) (i32.const 4096))
             (func (export "run_command") (param i32 i32) (result i64) {body}))"#
    )))
    .unwrap()
}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("notesec-plugins-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn the_example_wasm_is_built_from_its_wat() {
    let built = wat::parse_file(format!("{EXAMPLE}/plugin.wat")).unwrap();
    let path = format!("{EXAMPLE}/plugin.wasm");
    if std::env::var_os("NOTESEC_WRITE_EXAMPLE").is_some() {
        fs::write(&path, &built).unwrap();
    }
    assert_eq!(
        fs::read(&path).unwrap(),
        built,
        "rebuild: NOTESEC_WRITE_EXAMPLE=1 cargo test"
    );
}

#[test]
fn the_example_counts_words() {
    let p = load(Path::new(EXAMPLE)).unwrap();
    assert_eq!(
        (p.id.as_str(), p.render.as_deref()),
        ("word-count", Some("word-count"))
    );
    assert_eq!(p.commands[0].label, "Count words in this block");
    let c = compile(&fs::read(&p.wasm).unwrap()).unwrap();
    let input = command_input("count", "Page", "one two\nthree \"four\"\tfive", "");
    let out = call(&c, "run_command", input.as_bytes(), COMMAND_FUEL).unwrap();
    assert_eq!(
        parse_actions(&out.bytes).unwrap(),
        [Action::SetStatus("Words: 5".into())]
    );
    assert_eq!(out.log, ["word-count: counting"]);
    let out = call(&c, "render", render_input("", "").as_bytes(), RENDER_FUEL).unwrap();
    let lines = parse_render(&out.bytes).unwrap();
    assert_eq!(lines[0].text, "0 words");
}

fn write_plugin(root: &Path, folder: &str, manifest: &str, wasm: Option<&[u8]>) -> PathBuf {
    let dir = root.join("plugins").join(folder);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("plugin.toml"), manifest).unwrap();
    if let Some(w) = wasm {
        fs::write(dir.join("plugin.wasm"), w).unwrap();
    }
    dir
}

fn manifest(id: &str) -> String {
    format!("id = \"{id}\"\nname = \"N\"\nversion = \"1\"\napi_version = 1\n")
}

#[test]
fn manifests_are_checked() {
    let root = temp("manifests");
    let ok = wasm("(module)");
    write_plugin(&root, "good", &manifest("good"), Some(&ok));
    write_plugin(&root, "Bad_Id", &manifest("Bad_Id"), Some(&ok));
    write_plugin(&root, "other", &manifest("good"), Some(&ok));
    write_plugin(&root, "traversal", &manifest("../../etc"), Some(&ok));
    write_plugin(&root, "nowasm", &manifest("nowasm"), None);
    write_plugin(
        &root,
        "api",
        &manifest("api").replace("api_version = 1", "api_version = 2"),
        Some(&ok),
    );
    write_plugin(
        &root,
        "extra",
        &format!("{}evil = true\n", manifest("extra")),
        Some(&ok),
    );
    write_plugin(
        &root,
        "cmd",
        &format!(
            "{}[[commands]]\nid = \"../x\"\nlabel = \"L\"\n",
            manifest("cmd")
        ),
        Some(&ok),
    );
    let big = write_plugin(&root, "big", &manifest("big"), None);
    fs::write(big.join("plugin.wasm"), vec![0u8; MAX_WASM as usize + 1]).unwrap();
    let linked = write_plugin(&root, "linked", &manifest("linked"), None);
    std::os::unix::fs::symlink("/etc/hostname", linked.join("plugin.wasm")).unwrap();
    fs::write(root.join("plugins/stray.txt"), "x").unwrap();

    let (found, errors) = discover(&root);
    assert_eq!(
        found.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
        ["good"]
    );
    let why = |f: &str| {
        errors
            .iter()
            .find(|(n, _)| n == f)
            .map(|(_, e)| e.clone())
            .unwrap()
    };
    assert!(why("Bad_Id").contains("folder name"));
    assert!(why("other").contains("isn't its folder"));
    assert!(why("traversal").contains("isn't its folder"));
    assert!(why("nowasm").contains("no plugin.wasm"));
    assert!(why("api").contains("API 2"));
    assert!(why("extra").contains("evil"));
    assert!(why("cmd").contains("../x"));
    assert!(why("big").contains("too big"));
    assert!(why("linked").contains("regular file"));
    assert!(why("stray.txt").contains("not a folder"));
    for id in ["..", "a/b", "", "-x", "A", "a b"] {
        assert!(!valid_id(id), "{id}");
    }
    assert_eq!(found[0].hash, wasm_hash(&ok));
    assert_ne!(wasm_hash(&ok), wasm_hash(&wasm("(module (memory 1))")));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn only_host_log_may_be_imported() {
    let err = compile(&wasm(
        r#"(module (import "wasi_snapshot_preview1" "fd_write" (func (param i32 i32 i32 i32) (result i32))))"#,
    ));
    assert!(err.err().unwrap().contains("fd_write"));
    assert!(compile(b"not wasm").is_err());
}

#[test]
fn fuel_stops_an_endless_loop() {
    let c = plugin("(loop $l (br $l)) (i64.const 0)");
    let err = call(&c, "run_command", b"", 100_000).unwrap_err();
    assert!(err.contains("out of fuel"), "{err}");
    // A start function that loops is stopped too.
    let c = compile(&wasm(r#"(module (func $s (loop $l (br $l))) (start $s))"#)).unwrap();
    assert!(call(&c, "run_command", b"", 100_000)
        .unwrap_err()
        .contains("out of fuel"));
}

#[test]
fn memory_is_capped() {
    let pages = (MAX_MEMORY / 65536) as i32;
    let c = plugin(&format!(
        "(drop (memory.grow (i32.const {pages}))) (i64.const 0)"
    ));
    let err = call(&c, "run_command", b"", COMMAND_FUEL).unwrap_err();
    assert!(err.contains("more memory"), "{err}");
    let c = compile(&wasm(&format!(
        r#"(module (memory (export "memory") {}))"#,
        pages + 1
    )))
    .unwrap();
    assert!(call(&c, "run_command", b"", COMMAND_FUEL).is_err());
    // Within the cap is fine.
    let c = plugin("(drop (memory.grow (i32.const 10))) (i64.or (i64.shl (i64.const 100) (i64.const 32)) (i64.const 44))");
    assert!(call(&c, "run_command", b"", COMMAND_FUEL).is_ok());
}

#[test]
fn answers_outside_memory_or_too_big_are_refused() {
    let c = plugin("(i64.or (i64.shl (i64.const 65530) (i64.const 32)) (i64.const 100))");
    assert!(call(&c, "run_command", b"", COMMAND_FUEL)
        .unwrap_err()
        .contains("outside"));
    let c = plugin("(i64.const -1)");
    assert!(call(&c, "run_command", b"", COMMAND_FUEL)
        .unwrap_err()
        .contains("too big"));
    let c = compile(&wasm(
        r#"(module (memory (export "memory") 1)
             (func (export "alloc") (param i32) (result i32) (i32.const 70000))
             (func (export "run_command") (param i32 i32) (result i64) (i64.const 0)))"#,
    ))
    .unwrap();
    assert!(call(&c, "run_command", b"abc", COMMAND_FUEL)
        .unwrap_err()
        .contains("alloc"));
    let c = plugin("unreachable");
    assert!(call(&c, "run_command", b"", COMMAND_FUEL)
        .unwrap_err()
        .contains("crashed"));
    assert!(call(&c, "render", b"", COMMAND_FUEL)
        .unwrap_err()
        .contains("render"));
    let ok = plugin("(i64.or (i64.shl (i64.const 100) (i64.const 32)) (i64.const 44))");
    let out = call(&ok, "run_command", b"", COMMAND_FUEL).unwrap();
    assert_eq!(
        parse_actions(&out.bytes).unwrap(),
        [Action::SetStatus("hi".into())]
    );
}

#[test]
fn outputs_are_checked() {
    let parse = |s: &str| parse_actions(s.as_bytes());
    assert_eq!(
        parse("[[actions]]\ntype = \"insert_block\"\ntext = \"a\\nb\"\n[[actions]]\ntype = \"open_page\"\ntitle = \"P\"\n").unwrap(),
        [Action::InsertBlock("a\nb".into()), Action::OpenPage("P".into())]
    );
    assert_eq!(parse("").unwrap(), []);
    for bad in [
        "{\"actions\": []}",
        "[[actions]]\ntype = \"delete_everything\"\n",
        "[[actions]]\ntype = \"set_status\"\n",
        "[[actions]]\ntype = \"set_status\"\ntext = \"two\\nlines\"\n",
        "[[actions]]\ntype = \"replace_block\"\ntext = \"bell\\u0007\"\n",
        "[[actions]]\ntype = \"set_status\"\ntext = \"x\"\nscript = \"y\"\n",
    ] {
        assert!(parse(bad).is_err(), "{bad}");
    }
    let many = "[[actions]]\ntype = \"set_status\"\ntext = \"x\"\n".repeat(MAX_ACTIONS + 1);
    assert!(parse(&many).is_err());
    assert!(parse_actions(&[0xff, 0xfe]).is_err());
    let lines = parse_render(b"text = \"plain\\n**bold**\\n*it*\\n<b>no</b>\"").unwrap();
    assert_eq!(
        lines,
        [
            Line {
                text: "plain".into(),
                bold: false,
                italic: false
            },
            Line {
                text: "bold".into(),
                bold: true,
                italic: false
            },
            Line {
                text: "it".into(),
                bold: false,
                italic: true
            },
            Line {
                text: "<b>no</b>".into(),
                bold: false,
                italic: false
            },
        ]
    );
    assert!(parse_render(format!("text = \"{}\"", "x\\n".repeat(60)).as_bytes()).is_err());
}

#[test]
fn inputs_are_escaped_toml_with_the_block_first() {
    let input = command_input("c", "Pa\"ge", "a\\b\n\u{7}", "sel");
    assert!(input.starts_with("block = \"a\\\\b\\n\\u0007\"\n"));
    let t: toml::Table = input.parse().unwrap();
    assert_eq!(t["block"].as_str(), Some("a\\b\n\u{7}"));
    assert_eq!(t["page"].as_str(), Some("Pa\"ge"));
    assert_eq!(
        macro_calls(
            "x {{word-count}} {{ word-count  a b }} {{other}} {{word-countx}}",
            "word-count"
        ),
        ["", "a b"]
    );
}
