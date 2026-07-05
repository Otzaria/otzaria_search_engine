//! אכיפת היגיינת מקור: אין רווחי סוף-שורה באף קובץ Rust תחת `src/`.
//!
//! למה זה טסט ולא רק מוסכמה: תבנית ה-codegen של flutter_rust_bridge
//! פולטת רווח סופי בשורות ה-`wrap_normal` של `src/frb_generated.rs`
//! **בכל הרצה** של `flutter_rust_bridge_codegen generate`, ו-`git diff
//! --check` נכשל עליהם. בלי אכיפה, כל רגנרציה מחזירה את הבעיה בשקט.
//!
//! התיקון אחרי כל הרצת codegen (מתועד גם ב-`flutter_rust_bridge.yaml`):
//! ```sh
//! sed -i 's/[ \t]*$//' rust/src/frb_generated.rs
//! ```

use std::fs;
use std::path::Path;

#[test]
fn no_trailing_whitespace_in_rust_sources() {
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    collect_trailing_whitespace(&src_dir, &mut offenders);
    assert!(
        offenders.is_empty(),
        "trailing whitespace in Rust sources (the FRB codegen re-emits it in \
         frb_generated.rs on every `flutter_rust_bridge_codegen generate`; \
         clean with `sed -i 's/[ \\t]*$//' <file>`):\n{}",
        offenders.join("\n"),
    );
}

fn collect_trailing_whitespace(dir: &Path, offenders: &mut Vec<String>) {
    for entry in fs::read_dir(dir).expect("read source directory") {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            collect_trailing_whitespace(&path, offenders);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let content = fs::read_to_string(&path).expect("read source file");
            for (index, line) in content.lines().enumerate() {
                if line.ends_with(' ') || line.ends_with('\t') {
                    offenders.push(format!("{}:{}", path.display(), index + 1));
                }
            }
        }
    }
}
