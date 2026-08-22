//! `permissions_check` doctor tool tests (mock transport).

use personai_core::macos::MockTransport;

#[tokio::test]
async fn reports_each_target_app() {
    let mut t = MockTransport::new();
    // One probe per target app: Mail, Calendar, Messages (in order).
    for _ in 0..3 {
        t.enqueue(r#"{"ok":true,"value":1}"#);
    }
    let res = mcp_macos::permissions::check(&mut t).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&res).unwrap();
    let apps: Vec<&str> = v["permissions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["app"].as_str().unwrap())
        .collect();
    assert_eq!(apps, vec!["Mail", "Calendar", "Messages"]);
    assert!(
        v["permissions"].as_array().unwrap()[0]["ok"]
            .as_bool()
            .unwrap()
    );
}

#[tokio::test]
async fn denied_probe_includes_fix_hint() {
    let mut t = MockTransport::new();
    t.enqueue(r#"{"ok":true,"value":1}"#); // Mail ok
    t.enqueue(r#"{"ok":false,"error":{"number":-1743,"desc":"not allowed"}}"#); // Calendar denied
    t.enqueue(r#"{"ok":true,"value":1}"#); // Messages ok
    let res = mcp_macos::permissions::check(&mut t).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&res).unwrap();
    let cal = &v["permissions"][1];
    assert_eq!(cal["app"], "Calendar");
    assert_eq!(cal["ok"], false);
    let fix = cal["fix"].as_str().expect("denied entry carries a fix");
    assert!(fix.contains("Privacy & Security"), "{fix}");
}

#[tokio::test]
async fn overall_ok_false_when_any_denied() {
    let mut t = MockTransport::new();
    t.enqueue(r#"{"ok":false,"error":{"number":-1743,"desc":"denied"}}"#);
    t.enqueue(r#"{"ok":true,"value":1}"#);
    t.enqueue(r#"{"ok":true,"value":1}"#);
    let res = mcp_macos::permissions::check(&mut t).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&res).unwrap();
    assert_eq!(v["ok"], false);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn real_permissions_check() {
    use personai_core::macos::JxaTransport;
    // May pop TCC prompts on first run; that is the intended onboarding.
    let mut real = JxaTransport;
    let res = mcp_macos::permissions::check(&mut real).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&res).unwrap();
    assert!(v["permissions"].is_array());
}
