//! Regression coverage for quickwit-oss/tantivy#3086 and phrase unions.
use tantivy::collector::{Count, DocSetCollector, TopDocs};
use tantivy::query::{BooleanQuery, DisjunctionMaxQuery, Occur, PhraseQuery, Query, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, Schema, INDEXED, TEXT};
use tantivy::{doc, DocId, Index, IndexWriter, Term};

fn fixture(
    nonphrase_with_both_terms: bool,
) -> tantivy::Result<(tantivy::Searcher, Field, Field, Field)> {
    let mut schema = Schema::builder();
    let project = schema.add_u64_field("project", INDEXED);
    let title = schema.add_text_field("title", TEXT);
    let content = schema.add_text_field("content", TEXT);
    let index = Index::create_in_ram(schema.build());
    let mut writer: IndexWriter = index.writer_with_num_threads(1, 50_000_000)?;
    let filler = |writer: &mut IndexWriter, n: usize| -> tantivy::Result<()> {
        for _ in 0..n {
            writer.add_document(doc!(project => 999u64, content => "service team meeting"))?;
        }
        Ok(())
    };
    let both = |writer: &mut IndexWriter| {
        writer.add_document(doc!(project => 999u64, content => "service order"))
    };
    let title_hit = |writer: &mut IndexWriter| {
        writer.add_document(
            doc!(project => 7u64, title => "service order", content => "nothing here"),
        )
    };
    both(&mut writer)?; // 0
    filler(&mut writer, 5_999)?;
    title_hit(&mut writer)?; // 6000
    filler(&mut writer, 4_199)?;
    both(&mut writer)?; // 10200
    filler(&mut writer, 799)?;
    title_hit(&mut writer)?; // 11000
    filler(&mut writer, 9)?;
    let false_hit = if nonphrase_with_both_terms {
        "service purchase order"
    } else {
        "purchase order"
    };
    writer.add_document(doc!(project => 7u64, content => false_hit))?; // 11010: does not match either branch
    filler(&mut writer, 90)?;
    both(&mut writer)?; // 11101
    writer.commit()?;
    let searcher = index.reader()?.searcher();
    assert_eq!(searcher.segment_readers().len(), 1);
    Ok((searcher, project, title, content))
}

fn and_terms(field: Field) -> Box<dyn Query> {
    Box::new(BooleanQuery::new(
        ["service", "order"]
            .into_iter()
            .map(|word| {
                (
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(field, word),
                        IndexRecordOption::Basic,
                    )) as Box<dyn Query>,
                )
            })
            .collect(),
    ))
}

fn phrase(field: Field) -> Box<dyn Query> {
    Box::new(PhraseQuery::new(vec![
        Term::from_field_text(field, "service"),
        Term::from_field_text(field, "order"),
    ]))
}

fn category(project: Field) -> Box<dyn Query> {
    Box::new(TermQuery::new(
        Term::from_field_u64(project, 7),
        IndexRecordOption::Basic,
    ))
}

fn ids(searcher: &tantivy::Searcher, query: &dyn Query) -> tantivy::Result<Vec<DocId>> {
    let mut ids: Vec<_> = searcher
        .search(query, &DocSetCollector)?
        .into_iter()
        .map(|address| address.doc_id)
        .collect();
    ids.sort_unstable();
    Ok(ids)
}

#[test]
fn issue_3086_and_branches_behind_category_filter() -> tantivy::Result<()> {
    let (searcher, project, title, content) = fixture(false)?;
    let union: Box<dyn Query> = Box::new(DisjunctionMaxQuery::with_tie_breaker(
        vec![and_terms(content), and_terms(title)],
        0.0,
    ));
    let query = BooleanQuery::new(vec![(Occur::Must, category(project)), (Occur::Must, union)]);
    assert_eq!(ids(&searcher, &query)?, vec![6000, 11000]);
    Ok(())
}

#[test]
fn phrase_union_behind_category_filter_excludes_nonphrase() -> tantivy::Result<()> {
    let (searcher, project, title, content) = fixture(true)?;
    let union: Box<dyn Query> = Box::new(DisjunctionMaxQuery::with_tie_breaker(
        vec![phrase(content), phrase(title)],
        0.0,
    ));
    let query = BooleanQuery::new(vec![(Occur::Must, category(project)), (Occur::Must, union)]);
    assert_eq!(ids(&searcher, &query)?, vec![6000, 11000]);
    Ok(())
}

#[test]
fn category_inside_each_phrase_branch() -> tantivy::Result<()> {
    let (searcher, project, title, content) = fixture(true)?;
    let branch = |field| {
        Box::new(BooleanQuery::new(vec![
            (Occur::Must, phrase(field)),
            (Occur::Must, category(project)),
        ])) as Box<dyn Query>
    };
    let query = BooleanQuery::new(vec![
        (Occur::Should, branch(content)),
        (Occur::Should, branch(title)),
    ]);
    assert_eq!(ids(&searcher, &query)?, vec![6000, 11000]);
    Ok(())
}

#[test]
fn boolean_or_phrase_branches_behind_category_filter() -> tantivy::Result<()> {
    let (searcher, project, title, content) = fixture(true)?;
    let union = BooleanQuery::new(vec![
        (Occur::Should, phrase(content)),
        (Occur::Should, phrase(title)),
    ]);
    let query = BooleanQuery::new(vec![
        (Occur::Must, category(project)),
        (Occur::Must, Box::new(union)),
    ]);
    let found = ids(&searcher, &query)?;
    let count = searcher.search(&query, &Count)?;
    let mut scored: Vec<_> = searcher
        .search(&query, &TopDocs::with_limit(10).order_by_score())?
        .into_iter()
        .map(|(_, address)| address.doc_id)
        .collect();
    scored.sort_unstable();
    assert_eq!(scored, vec![6000, 11000]);
    assert_eq!(count, 2);
    assert_eq!(found, vec![6000, 11000]);
    Ok(())
}
