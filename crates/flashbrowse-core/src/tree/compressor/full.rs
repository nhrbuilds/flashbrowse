use super::*;

/// Full tree: every non-ignored node, document order, standard action IDs.
pub(super) fn compress_full(nodes: &[AXNode]) -> (Vec<SemanticNode>, ActionMap) {
    let mut out: Vec<SemanticNode> = Vec::with_capacity(nodes.len());
    let mut map: ActionMap = HashMap::new();
    let mut next_id = Some(1u32);

    let mut parent_of: HashMap<&str, &str> = HashMap::new();
    let mut role_of: HashMap<&str, String> = HashMap::with_capacity(nodes.len());
    let mut children_of: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut index: HashMap<&str, &AXNode> = HashMap::with_capacity(nodes.len());
    for n in nodes {
        role_of.insert(n.node_id.as_str(), ax_role(n));
        if index.insert(n.node_id.as_str(), n).is_some() {
            return render_unlinked_nodes(nodes);
        }
        for child in &n.child_ids {
            parent_of.insert(child.as_str(), n.node_id.as_str());
            children_of
                .entry(n.node_id.as_str())
                .or_default()
                .push(child.as_str());
        }
    }

    let Some(depths) = forest_depths(nodes, &parent_of) else {
        return render_unlinked_nodes(nodes);
    };
    let ordered = document_order(nodes, &children_of, &parent_of);

    let mut id_of_node: HashMap<&str, u32> = HashMap::new();
    for node in &ordered {
        if node.ignored {
            continue;
        }
        let role = role_of
            .get(node.node_id.as_str())
            .map(String::as_str)
            .unwrap_or("");
        if !ACTIONABLE_ROLES.contains(&role) {
            continue;
        }
        let Some(backend_node_id) = node.backend_node_id else {
            continue;
        };
        let Some(id) = next_id else { break };
        next_id = id.checked_add(1);
        map.insert(id, backend_node_id);
        id_of_node.insert(node.node_id.as_str(), id);
    }

    for node in &ordered {
        if node.ignored {
            continue;
        }
        let role = role_of
            .get(node.node_id.as_str())
            .map(String::as_str)
            .unwrap_or("");
        let mut name = ax_name(node);
        if name.is_empty() && derives_name_from_children(role) {
            name = derive_name(node.node_id.as_str(), &children_of, &index);
        }
        let depth = depths
            .get(node.node_id.as_str())
            .copied()
            .unwrap_or_default();
        let extras = ax_extras(node, role);
        if ACTIONABLE_ROLES.contains(&role) {
            if let Some(&id) = id_of_node.get(node.node_id.as_str()) {
                if name.is_empty() {
                    name = "(unlabeled)".to_string();
                }
                out.push(SemanticNode {
                    action_id: Some(id),
                    role: role.to_string(),
                    name,
                    extras,
                    level: ax_level(node),
                    node_id: node.node_id.clone(),
                    depth,
                });
                continue;
            }
        }
        out.push(SemanticNode {
            action_id: None,
            level: ax_level(node),
            role: role.to_string(),
            name,
            extras,
            node_id: node.node_id.clone(),
            depth,
        });
    }

    (out, map)
}
