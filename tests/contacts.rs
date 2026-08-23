//! Contacts toolset contract tests. All run on every OS via `MockTransport`.

use mcp_macos::contacts::ContactsToolset;
use personai_core::macos::MockTransport;
use serde_json::Value;

fn fixture(envelopes: &[&str]) -> ContactsToolset<MockTransport> {
    let mut t = MockTransport::new();
    for e in envelopes {
        t.enqueue(e);
    }
    ContactsToolset::new(t)
}

fn parse(s: &str) -> Value {
    serde_json::from_str(s).unwrap()
}

#[tokio::test]
async fn search_escapes_query_and_hydrates_page() {
    let env = r#"{"ok":true,"value":{"total":1,"offset":2,"limit":5,"contacts":[{"id":"i1","name":"Mom Khatod","organization":"","emails":["mom@x.com"],"phones":["+1 555"]}]}}"#;
    let mut f = fixture(&[env]);
    let v = parse(&f.search("O'Brien", Some(5), 2).await.unwrap());
    assert_eq!(v["total"], 1);
    assert_eq!(v["contacts"][0]["name"], "Mom Khatod");
    let script = &f.transport.calls()[0].script;
    // Query lowercased and escaped into the JS string literal.
    assert!(
        script.contains(r#""o'brien""#),
        "query not escaped: {script}"
    );
    // Pagination clauses baked in.
    assert!(script.contains("Math.min(2 + 5"), "{script}");
}

#[tokio::test]
async fn empty_query_is_census_mode_with_default_limit() {
    let env = r#"{"ok":true,"value":{"total":0,"offset":0,"limit":20,"contacts":[]}}"#;
    let mut f = fixture(&[env]);
    let v = parse(&f.search("", None, 0).await.unwrap());
    assert_eq!(v["total"], 0);
    let script = &f.transport.calls()[0].script;
    assert!(
        script.contains("q === ''"),
        "empty query must match-all: {script}"
    );
    assert!(script.contains("Math.min(0 + 20"), "{script}");
    assert_eq!(
        f.transport.calls().len(),
        1,
        "one bulk-fetch script per call"
    );
}
