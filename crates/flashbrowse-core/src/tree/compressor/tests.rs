use flashbrowse_cdp::protocol::accessibility::{AXNode, AXProperty, AXValue};

use super::compress;

fn val(s: &str) -> AXValue {
    AXValue {
        type_: Some("ComputedString".to_string()),
        value: Some(serde_json::Value::String(s.to_string())),
    }
}
fn role(s: &str) -> AXValue {
    AXValue {
        type_: Some("RoleValue".to_string()),
        value: Some(serde_json::Value::String(s.to_string())),
    }
}
fn node(id: &str, role_s: &str, name: &str, backend: Option<i64>) -> AXNode {
    AXNode {
        node_id: id.to_string(),
        ignored: false,
        role: Some(role(role_s)),
        name: Some(val(name)),
        value: None,
        backend_node_id: backend,
        child_ids: vec![],
        ignored_reasons: None,
        properties: None,
    }
}
fn with_children(mut n: AXNode, children: &[&str]) -> AXNode {
    n.child_ids = children.iter().map(|s| s.to_string()).collect();
    n
}

#[test]
fn cyclic_or_duplicate_links_preserve_non_ignored_text() {
    let input = serde_json::json!([
        {"nodeId":"1","ignored":false,"role":{"value":"button"},"name":{"value":"First"},"childIds":["2"],"backendDOMNodeId":3},
        {"nodeId":"2","ignored":false,"role":{"value":"button"},"name":{"value":"Second"},"childIds":["1"],"backendDOMNodeId":4}
    ]);
    let mut input: Vec<AXNode> = serde_json::from_value(input).unwrap();
    let (out, actions) = compress(&input);
    assert_eq!(
        out.iter()
            .map(|node| node.name.as_str())
            .collect::<Vec<_>>(),
        ["First", "Second"]
    );
    assert!(actions.is_empty());
    input[1].node_id = input[0].node_id.clone();
    let (out, actions) = compress(&input);
    assert_eq!(out.len(), 2);
    assert!(actions.is_empty());
}

#[test]
fn text_inside_a_dropped_layout_cell_survives() {
    // Hacker News footer: the LayoutTableCell's name is the concatenation
    // of its children and the cell itself is dropped, so its StaticText
    // child is NOT a duplicate. Treating it as one lost "Search:".
    let cell = with_children(
        node("1", "layouttablecell", "Guidelines | Search:", Some(1)),
        &["2", "3"],
    );
    let link = with_children(node("2", "link", "Guidelines", Some(2)), &["4"]);
    let label = node("3", "statictext", "Search:", None);
    let link_text = node("4", "statictext", "Guidelines", None);
    let (out, _) = compress(&[cell, link, label, link_text]);
    let names: Vec<&str> = out.iter().map(|n| n.name.as_str()).collect();
    assert!(names.contains(&"Search:"), "label dropped: {names:?}");
    assert_eq!(
        names.iter().filter(|n| **n == "Guidelines").count(),
        1,
        "link text duplicated: {names:?}"
    );
}

#[test]
fn full_tree_is_a_superset_of_pruned_nodes() {
    let cell = with_children(
        node("1", "layouttablecell", "Guidelines | Search:", Some(1)),
        &["2", "3"],
    );
    let link = with_children(node("2", "link", "Guidelines", Some(2)), &["4"]);
    let label = node("3", "statictext", "Search:", None);
    let link_text = node("4", "statictext", "Guidelines", None);
    let nodes = [cell, link, label, link_text];
    let (pruned, _) = super::compress_pruned(&nodes);
    let (full, _) = super::compress_full(&nodes);
    assert!(
        full.len() >= pruned.len(),
        "full={} pruned={}",
        full.len(),
        pruned.len()
    );
}

#[test]
fn slider_and_spinbutton_render_their_value() {
    let mut slider = node("1", "slider", "Zoom", Some(1));
    slider.value = Some(val("50"));
    let mut spin = node("2", "spinbutton", "Quantity", Some(2));
    spin.value = Some(val("3"));
    let (out, map) = compress(&[slider, spin]);
    assert_eq!(map.len(), 2, "both controls are actionable");
    assert!(out[0].extras.contains(&"value=\"50\"".to_string()));
    assert!(out[1].extras.contains(&"value=\"3\"".to_string()));
}

#[test]
fn prune_ignored_and_empty_structural() {
    let nodes = vec![
        AXNode {
            node_id: "1".to_string(),
            ignored: true,
            role: Some(role("button")),
            name: Some(val("X")),
            value: None,
            backend_node_id: Some(1),
            child_ids: vec![],
            ignored_reasons: None,
            properties: None,
        },
        node("2", "generic", "", None),
        node("3", "button", "Submit", Some(10)),
    ];
    let (out, map) = compress(&nodes);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].action_id, Some(1));
    assert_eq!(map.get(&1), Some(&10));
}

#[test]
fn assigns_sequential_ids_depth_first() {
    let nodes = vec![
        node("1", "link", "Home", Some(1)),
        node("2", "button", "Go", Some(2)),
        node("3", "heading", "Hi", None),
    ];
    let (out, map) = compress(&nodes);
    assert_eq!(out[0].action_id, Some(1));
    assert_eq!(out[1].action_id, Some(2));
    assert_eq!(out[2].action_id, None);
    assert_eq!(out[2].role, "heading");
    assert_eq!(map.len(), 2);
}

#[test]
fn unnumbered_without_backend() {
    let nodes = vec![node("1", "button", "Orphan", None)];
    let (out, map) = compress(&nodes);
    assert!(out[0].action_id.is_none());
    assert!(map.is_empty());
}

#[test]
fn heading_level_read_from_properties() {
    let mut h3 = node("1", "heading", "Three", None);
    h3.properties = Some(vec![AXProperty {
        name: "level".to_string(),
        value: Some(AXValue {
            type_: Some("integer".to_string()),
            value: Some(serde_json::Value::from(3)),
        }),
    }]);
    let (out, _) = compress(&[h3]);
    assert_eq!(out[0].level, Some(3));
}

#[test]
fn heading_text_not_duplicated_by_statictext() {
    let nodes = vec![
        with_children(node("1", "heading", "Three", None), &["2"]),
        node("2", "statictext", "Three", Some(5)),
    ];
    let (out, _) = compress(&nodes);
    assert_eq!(out.len(), 1, "statictext dupe must be dropped");
    assert_eq!(out[0].role, "heading");
    assert_eq!(out[0].name, "Three");
}

#[test]
fn input_value_is_reported() {
    let mut tb = node("1", "textbox", "Name", Some(9));
    tb.value = Some(val("oldv"));
    let (out, _) = compress(&[tb]);
    assert!(out[0].extras.iter().any(|e| e == "value=\"oldv\""));
}

#[test]
fn table_row_keeps_cell_text() {
    // table > rowgroup > row > cell > statictext
    let nodes = vec![
        with_children(node("1", "table", "", None), &["2"]),
        with_children(node("2", "rowgroup", "", None), &["3"]),
        with_children(node("3", "row", "", None), &["4", "6"]),
        with_children(node("4", "cell", "alpha", None), &["5"]),
        node("5", "statictext", "alpha", None),
        with_children(node("6", "cell", "ok", None), &["7"]),
        node("7", "statictext", "ok", None),
    ];
    let (out, _) = compress(&nodes);
    let row = out
        .iter()
        .find(|n| n.role == "row")
        .expect("the row must be emitted");
    assert_eq!(row.name, "alpha | ok");
    // The cell text must NOT be dropped, and must NOT be duplicated.
    let text: Vec<&str> = out.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(text.iter().filter(|t| t.contains("alpha")).count(), 1);
}

#[test]
fn table_row_keeps_text_when_cell_has_no_name() {
    // A cell whose accessible name is empty but which owns the text.
    let nodes = vec![
        with_children(node("1", "table", "", None), &["2"]),
        with_children(node("2", "row", "", None), &["3"]),
        with_children(node("3", "cell", "", None), &["4"]),
        node("4", "statictext", "gamma", None),
    ];
    let (out, _) = compress(&nodes);
    let row = out.iter().find(|n| n.role == "row").expect("row");
    assert_eq!(row.name, "gamma");
}

#[test]
fn list_item_keeps_its_text() {
    // list > listitem > statictext
    let nodes = vec![
        with_children(node("1", "list", "", None), &["2"]),
        with_children(node("2", "listitem", "", None), &["3"]),
        node("3", "statictext", "Plain item", None),
    ];
    let (out, _) = compress(&nodes);
    assert!(
        out.iter().any(|n| n.name == "Plain item"),
        "list item text must survive: {:?}",
        out.iter().map(|n| &n.name).collect::<Vec<_>>()
    );
}

#[test]
fn output_is_in_document_order() {
    // Chromium returns the AX tree breadth-first, so the raw array is
    // [parent, childA, childB, grandchildA, grandchildB]. Reading the
    // raw order would put "First" before "Second" only by accident;
    // the tree says First comes first, and so must the output.
    let nodes = vec![
        with_children(node("1", "generic", "", None), &["2", "3"]),
        with_children(node("2", "heading", "First", None), &["4"]),
        with_children(node("3", "heading", "Second", None), &["5"]),
        node("4", "statictext", "First", None),
        node("5", "statictext", "Second", None),
    ];
    let (out, _) = compress(&nodes);
    let names: Vec<&str> = out.iter().map(|n| n.name.as_str()).collect();
    let first = names.iter().position(|n| *n == "First").expect("First");
    let second = names.iter().position(|n| *n == "Second").expect("Second");
    assert!(first < second, "document order broken: {names:?}");
}

#[test]
fn actionable_descendant_is_not_duplicated_in_parent_text() {
    // A list item containing a link: the link renders itself with an
    // action ID, so the list item must not repeat its text.
    let nodes = vec![
        with_children(node("1", "list", "", None), &["2"]),
        with_children(node("2", "listitem", "", None), &["3"]),
        with_children(node("3", "link", "Linked item", Some(42)), &["4"]),
        node("4", "statictext", "Linked item", None),
    ];
    let (out, map) = compress(&nodes);
    assert_eq!(map.get(&1), Some(&42));
    let link_lines = out.iter().filter(|n| n.action_id.is_some()).count();
    assert_eq!(link_lines, 1);
    // The link text must appear exactly once in the whole output.
    let occurrences = out
        .iter()
        .filter(|n| n.name.contains("Linked item"))
        .count();
    assert_eq!(occurrences, 1, "link text duplicated: {out:?}");
}

#[test]
fn clickable_non_aria_div_gets_action_id_without_duplicating_text_or_wrapping_buttons() {
    let mut clickable_div = node("2", "generic", "", Some(20));
    clickable_div.properties = Some(vec![AXProperty {
        name: "clickable".to_string(),
        value: Some(AXValue {
            type_: Some("boolean".to_string()),
            value: Some(serde_json::Value::Bool(true)),
        }),
    }]);
    let mut wrapper_div = node("4", "generic", "", Some(40));
    wrapper_div.properties = Some(vec![AXProperty {
        name: "clickable".to_string(),
        value: Some(AXValue {
            type_: Some("boolean".to_string()),
            value: Some(serde_json::Value::Bool(true)),
        }),
    }]);
    let nodes = vec![
        with_children(node("1", "generic", "", None), &["2", "4"]),
        with_children(clickable_div, &["3"]),
        node("3", "statictext", "Custom Div", None),
        with_children(wrapper_div, &["5"]),
        with_children(node("5", "button", "Real Button", Some(50)), &["6"]),
        node("6", "statictext", "Real Button", None),
    ];
    let (out, map) = compress(&nodes);
    assert_eq!(map.get(&1), Some(&20));
    assert_eq!(map.get(&2), Some(&50));
    assert_eq!(map.len(), 2, "wrapper div must not duplicate button action");
    assert_eq!(
        out.len(),
        2,
        "child statictext must not be duplicated: {out:?}"
    );
    assert_eq!(out[0].action_id, Some(1));
    assert_eq!(out[0].role, "clickable");
    assert_eq!(out[0].name, "Custom Div");
    assert_eq!(out[1].action_id, Some(2));
    assert_eq!(out[1].role, "button");
    assert_eq!(out[1].name, "Real Button");
}
