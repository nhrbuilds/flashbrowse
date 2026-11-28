use super::*;

pub(super) fn compress_pruned(nodes: &[AXNode]) -> (Vec<SemanticNode>, ActionMap) {
    let mut out: Vec<SemanticNode> = Vec::with_capacity(nodes.len() / 2);
    let mut map: ActionMap = HashMap::new();
    let mut next_id = Some(1u32);

    // Indexes (single pass, no per-node re-computation in the hot loop).
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
    // Parent roles that render their own name (so child StaticText is a dupe).
    let mut name_bearing_parent: HashMap<&str, bool> = HashMap::with_capacity(nodes.len());
    for n in nodes {
        let role = role_of
            .get(n.node_id.as_str())
            .map(String::as_str)
            .unwrap_or("");
        name_bearing_parent.insert(
            n.node_id.as_str(),
            NAME_FROM_CHILDREN_ROLES.contains(&role) || role == "statictext",
        );
    }

    let mut seen: HashSet<&str> = HashSet::with_capacity(nodes.len());

    // Chromium returns the AX tree breadth-first; restore document order so
    // the agent reads the page the way it is laid out.
    let Some(depths) = forest_depths(nodes, &parent_of) else {
        return render_unlinked_nodes(nodes);
    };
    let ordered = document_order(nodes, &children_of, &parent_of);

    // Bottom-up pass: identify visible non-ARIA elements with explicit click
    // handlers (`clickable: true`), avoiding wrappers around already-actionable
    // descendants and recording which StaticText nodes they absorb.
    let mut clickable_non_aria: HashMap<&str, String> = HashMap::new();
    let mut suppressed_by_clickable: HashSet<&str> = HashSet::new();
    {
        let mut subtree_has_actionable: HashSet<&str> = HashSet::new();
        for node in ordered.iter().rev() {
            let id = node.node_id.as_str();
            let role = role_of.get(id).map(String::as_str).unwrap_or("");
            let self_aria_actionable =
                !node.ignored && ACTIONABLE_ROLES.contains(&role) && node.backend_node_id.is_some();
            let child_has_actionable = children_of
                .get(id)
                .is_some_and(|kids| kids.iter().any(|k| subtree_has_actionable.contains(*k)));
            if !child_has_actionable
                && !self_aria_actionable
                && !node.ignored
                && node.backend_node_id.is_some()
                && is_clickable_non_aria_role(role)
                && node.prop_bool("clickable")
            {
                let own = ax_name(node);
                if !own.is_empty() {
                    let (_, sources) = derive_name_with_sources(id, &children_of, &index);
                    for src in sources {
                        if let Some(src_node) = index.get(src) {
                            let src_text = ax_name(src_node);
                            if !src_text.is_empty() && own.contains(&src_text) {
                                suppressed_by_clickable.insert(src);
                            }
                        }
                    }
                    clickable_non_aria.insert(id, own);
                    subtree_has_actionable.insert(id);
                } else {
                    let (derived, sources) = derive_name_with_sources(id, &children_of, &index);
                    if !derived.is_empty() {
                        for src in sources {
                            suppressed_by_clickable.insert(src);
                        }
                        clickable_non_aria.insert(id, derived);
                        subtree_has_actionable.insert(id);
                    }
                }
            } else if self_aria_actionable || child_has_actionable {
                subtree_has_actionable.insert(id);
            }
        }
    }

    // ---- Pass 1: assign action IDs -------------------------------------
    //
    // The IDs must be known before the tree is rendered, because a table
    // row renders an actionable cell as `[N]` (referencing the link's own
    // numbered line) instead of repeating its text.
    let mut id_of_node: HashMap<&str, u32> = HashMap::new();
    {
        let mut pass_seen: HashSet<&str> = HashSet::with_capacity(nodes.len());
        for node in &ordered {
            if node.ignored {
                continue;
            }
            if !pass_seen.insert(node.node_id.as_str()) {
                continue;
            }
            let role = role_of
                .get(node.node_id.as_str())
                .map(String::as_str)
                .unwrap_or("");
            if !ACTIONABLE_ROLES.contains(&role)
                && !clickable_non_aria.contains_key(node.node_id.as_str())
            {
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
    }

    // ---- Pass 2: render -------------------------------------------------
    //
    // Table rows are rendered as ONE line (`a | b | c`), so the cell nodes
    // themselves must not be emitted again.
    let mut row_texts: HashMap<&str, String> = HashMap::new();
    let mut covered_by_row: HashSet<&str> = HashSet::new();
    for node in &ordered {
        if node.ignored {
            continue;
        }
        let role = role_of
            .get(node.node_id.as_str())
            .map(String::as_str)
            .unwrap_or("");
        if role != "row" {
            continue;
        }
        let Some(text) = row_text(node, &role_of, &index, &children_of, &id_of_node) else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        row_texts.insert(node.node_id.as_str(), text);
        for cid in &node.child_ids {
            let crole = role_of.get(cid.as_str()).map(String::as_str).unwrap_or("");
            if ROW_CELL_ROLES.contains(&crole) {
                covered_by_row.insert(cid.as_str());
            }
        }
    }

    for node in &ordered {
        if node.ignored {
            continue;
        }
        if !seen.insert(node.node_id.as_str()) {
            continue;
        }
        // A cell rendered as part of its row line: emitting it again would
        // duplicate the text.
        if covered_by_row.contains(node.node_id.as_str()) {
            continue;
        }
        let role = role_of
            .get(node.node_id.as_str())
            .map(String::as_str)
            .unwrap_or("");
        let mut name = ax_name(node);
        if name.is_empty() && derives_name_from_children(role) {
            // Chromium reports an empty accessible name for some links
            // whose text is owned by child nodes. Derive it so the agent
            // can see what the element actually opens.
            name = derive_name(node.node_id.as_str(), &children_of, &index);
        }

        // An image that repeats an ancestor's accessible name (an icon link
        // whose only content is the image) adds a duplicate line, not content.
        if role == "image"
            && !name.is_empty()
            && ancestor_contains(
                &parent_of,
                &index,
                &role_of,
                node.node_id.as_str(),
                &name,
                3,
            )
        {
            continue;
        }

        // InlineTextBox is pure layout noise - always drop.
        if role == "inlinetextbox" {
            continue;
        }
        if suppressed_by_clickable.contains(node.node_id.as_str()) {
            continue;
        }
        if let Some(clickable_name) = clickable_non_aria.get(node.node_id.as_str()) {
            if let Some(id) = id_of_node.get(node.node_id.as_str()) {
                let emit_role = if role == "image" {
                    "image".to_string()
                } else {
                    "clickable".to_string()
                };
                out.push(SemanticNode {
                    action_id: Some(*id),
                    role: emit_role,
                    name: clickable_name.clone(),
                    extras: ax_extras(node, role),
                    level: None,
                    node_id: node.node_id.clone(),
                    depth: depths
                        .get(node.node_id.as_str())
                        .copied()
                        .unwrap_or_default(),
                });
                continue;
            }
        }
        // StaticText duplicating an ancestor's accessible name (e.g. a table
        // cell whose name is the concatenation of its children).
        if role == "statictext" {
            if let Some(pid) = parent_of.get(node.node_id.as_str()) {
                if name_bearing_parent.get(pid).copied().unwrap_or(false) {
                    continue;
                }
                // Text already present in an ancestor's name -> redundant.
                if ancestor_contains(&parent_of, &index, &role_of, pid, &name, 3) {
                    continue;
                }
                // A control's value is already rendered as `value="..."`.
                if ancestor_value_matches(&parent_of, &index, node.node_id.as_str(), &name, 3) {
                    continue;
                }
            }
        }

        // Table row: emit ONE line for the whole row (`a | b | c`).
        // The row's own accessible name is empty in Chromium, so without
        // this the cell text would be lost entirely.
        if role == "row" {
            if let Some(text) = row_texts.get(node.node_id.as_str()) {
                out.push(SemanticNode {
                    action_id: None,
                    level: None,
                    role: "row".to_string(),
                    name: text.clone(),
                    extras: Vec::new(),
                    node_id: node.node_id.clone(),
                    depth: depths
                        .get(node.node_id.as_str())
                        .copied()
                        .unwrap_or_default(),
                });
            }
            // The cells themselves are emitted as part of the row line.
            continue;
        }
        // A table's accessible name is the concatenation of all its rows;
        // the rows are emitted individually -> drop the duplicate.
        if role == "table" && !node.child_ids.is_empty() {
            continue;
        }
        // `rowgroup` only groups rows - the rows carry the content.
        // (Chromium computes a rowgroup's name as the concatenation of its
        // rows, so keeping it would duplicate every row.)
        if role == "rowgroup" && !node.child_ids.is_empty() {
            continue;
        }
        // `listmarker` is a pure bullet glyph - decoration, not content.
        if role == "listmarker" {
            continue;
        }
        // Landmark roles (main/banner/navigation/...) carry a name that is
        // the concatenation of their whole subtree - the children are
        // emitted individually, so we keep only the structural label.
        if LANDMARK_ROLES.contains(&role) && !node.child_ids.is_empty() {
            out.push(SemanticNode {
                action_id: None,
                level: None,
                role: role.to_string(),
                name: String::new(),
                extras: Vec::new(),
                node_id: node.node_id.clone(),
                depth: depths
                    .get(node.node_id.as_str())
                    .copied()
                    .unwrap_or_default(),
            });
            continue;
        }
        // Layout containers (Chromium's internal table roles): their name is
        // just the concatenation of their children, which we emit
        // individually - emitting the container name duplicates every child.
        if LAYOUT_CONTAINER_ROLES.contains(&role) && !node.child_ids.is_empty() {
            continue;
        }
        // Pure structure (list items): no label, no name - the children
        // carry everything worth emitting.
        if STRUCTURE_ONLY_ROLES.contains(&role) && !node.child_ids.is_empty() {
            continue;
        }
        // A StaticText that owns *meaningful* children is a concatenation of
        // them (Chromium emits the whole subtree text on the container) - the
        // children are emitted individually, so the parent is a duplicate.
        //
        // The InlineTextBox child is excluded: in Chromium EVERY StaticText
        // owns one, so treating it as "has children" dropped every piece of
        // text on the page.
        if role == "statictext" && has_meaningful_child(node, &role_of) {
            continue;
        }
        // Pure punctuation (" | ", ".", "-") is decoration, not content.
        if role == "statictext"
            && !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_punctuation() || c.is_whitespace())
            && name.chars().any(|c| c.is_ascii_punctuation())
        {
            continue;
        }

        let is_actionable_role = ACTIONABLE_ROLES.contains(&role);
        let is_structural = STRUCTURAL_ROLES.contains(&role);
        let is_content = CONTENT_ROLES.contains(&role);

        // Prune empty structural nodes entirely - no text, cannot be acted on.
        if is_structural && name.is_empty() {
            continue;
        }
        // Prune empty content shells (no name payload).
        if is_content && name.is_empty() && role != "heading" {
            continue;
        }
        // Whitespace-only names on unknown roles.
        if !is_actionable_role && name.is_empty() && !is_content && !is_structural {
            continue;
        }

        let extras = ax_extras(node, role);

        // Actionable + backend handle -> numbered line.
        if is_actionable_role {
            if name.is_empty() {
                // An unnamed link cannot be reasoned about by an agent
                // (icon/anchor without text) -> drop it. Unnamed buttons
                // stay (icon buttons are meaningful) but are marked.
                if role == "link" {
                    if let Some(&id) = id_of_node.get(node.node_id.as_str()) {
                        map.remove(&id);
                    }
                    continue;
                }
                name = "(unlabeled)".to_string();
            }
            if let Some(id) = id_of_node.get(node.node_id.as_str()) {
                out.push(SemanticNode {
                    action_id: Some(*id),
                    role: role.to_string(),
                    name,
                    extras,
                    level: None,
                    node_id: node.node_id.clone(),
                    depth: depths
                        .get(node.node_id.as_str())
                        .copied()
                        .unwrap_or_default(),
                });
                continue;
            }
            // No backend id: fall through to unnumbered content (still useful text).
        }

        // Structural nodes that hold text become plain `text` lines.
        // `statictext` is plain text too - "Statictext: foo" would be noise
        // in the agent's prompt.
        let role = if is_structural || role == "statictext" {
            "text"
        } else {
            role
        };
        if name.is_empty() && role != "heading" {
            continue;
        }
        out.push(SemanticNode {
            action_id: None,
            level: ax_level(node),
            role: role.to_string(),
            name,
            extras,
            node_id: node.node_id.clone(),
            depth: depths
                .get(node.node_id.as_str())
                .copied()
                .unwrap_or_default(),
        });
    }

    (out, map)
}
