//! Syntax gate: every Lua snippet the server can send to the editor
//! must parse. Uses the estate's Lua 5.4.8 `luac` when present
//! (`PERISCOPE_LUAC` overrides the location); skips with a printed
//! note when no luac exists, so the rest of the suite still runs —
//! the primo gate is where this check is mandatory.

use std::path::PathBuf;
use std::process::Command;

fn locate_luac() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PERISCOPE_LUAC") {
        let candidate = PathBuf::from(path);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let candidate = PathBuf::from(home).join("workspace/tools/lua-5.4.8/src/luac");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    if let Ok(output) = Command::new("luac").arg("-v").output()
        && output.status.success()
    {
        return Some(PathBuf::from("luac"));
    }
    None
}

#[test]
fn all_snippets_parse_as_lua() {
    let Some(luac) = locate_luac() else {
        eprintln!("lua_syntax: no luac available, skipping (mandatory on primo)");
        return;
    };
    for (index, snippet) in periscope::lua::all_snippets().iter().enumerate() {
        let path = std::env::temp_dir().join(format!(
            "periscope-snippet-{}-{index}.lua",
            std::process::id()
        ));
        std::fs::write(&path, snippet).expect("write snippet");
        let output = Command::new(&luac)
            .arg("-p")
            .arg(&path)
            .output()
            .expect("run luac");
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let _ = std::fs::remove_file(&path);
        assert!(
            output.status.success(),
            "snippet {index} failed to parse: {stderr}"
        );
    }
}
