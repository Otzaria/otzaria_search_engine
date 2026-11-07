#[path = "contract.rs"]
mod contract;
use search_engine::api::search_engine::{BookOrderEntry, SearchEngine, TextStorage};
fn main() {
    let path = std::env::args().nth(1).expect("fixture path");
    std::fs::create_dir_all(&path).unwrap();
    let mut writer = SearchEngine::new(&path);
    writer.set_economy_indexing(true).unwrap();
    for (file, title, topic, text) in [
        (
            "id:101",
            "Device A",
            "/devices",
            "<h1>חומרה</h1>\nמחשב מהיר\nמסך כחול\nמחשב קטן",
        ),
        (
            "id:202",
            "Device B",
            "/devices",
            "מחשב מהיר\nמַחְשֵׁב נוח\nמסך ירוק",
        ),
        (
            "id:303",
            "Accessory",
            "/accessories",
            "מקלדת שחורה\nמחשב קטן",
        ),
    ] {
        writer
            .add_text_book(
                title.into(),
                topic.into(),
                file.into(),
                text.into(),
                None,
                TextStorage::InIndex,
                None,
                None,
            )
            .unwrap();
    }
    writer.commit().unwrap();
    writer
        .set_book_order(vec![
            BookOrderEntry {
                file_path: "id:202".into(),
                catalogue_order: 0,
                generation_order: 2,
            },
            BookOrderEntry {
                file_path: "id:303".into(),
                catalogue_order: 1,
                generation_order: 0,
            },
            BookOrderEntry {
                file_path: "id:101".into(),
                catalogue_order: 2,
                generation_order: 1,
            },
        ])
        .unwrap();
    let output = contract::contract(&writer);
    assert_eq!(output["pages"].as_array().unwrap().len(), 16);
    println!("{}", serde_json::to_string_pretty(&output).unwrap());
    drop(writer);
}
