//! Bounds for plugin-owned semantic trees before they reach native accessibility APIs.
use crate::types::AccessibleNode;
use std::collections::HashMap;

pub fn validate(nodes: &[AccessibleNode]) -> Result<(), String> {
    if nodes.len() > 4096 {
        return Err("accessibility: at most 4096 nodes per frame".into());
    }
    let mut depths = HashMap::new();
    let mut bytes = 0usize;
    let mut focused = false;
    for node in nodes {
        if node.id.is_empty() || node.id.len() > 16384 || node.id.contains('\0') || depths.contains_key(&node.id) {
            return Err("accessibility: IDs must be unique, nonempty, and at most 16384 bytes".into());
        }
        let depth = match &node.parent {
            None => 0,
            Some(parent) => depths.get(parent).copied().ok_or("accessibility: parents must precede children")? + 1,
        };
        if depth > 64 {
            return Err("accessibility: tree is too deep".into());
        }
        for text in
            [Some(&node.id), node.parent.as_ref(), Some(&node.label), node.description.as_ref(), node.value.as_ref()]
                .into_iter()
                .flatten()
        {
            if text.len() > 16384 {
                return Err("accessibility: text property exceeds 16384 bytes".into());
            }
            bytes += text.len();
        }
        if bytes > 256 * 1024 {
            return Err("accessibility: text budget exceeds 256 KiB".into());
        }
        if node.focused {
            if focused || node.disabled {
                return Err("accessibility: exactly one enabled node may have focus".into());
            }
            focused = true;
        }
        if node.actions.len() > 6 {
            return Err("accessibility: too many actions".into());
        }
        if node.position == Some(0) || node.position.zip(node.set_size).is_some_and(|(p, n)| p > n) {
            return Err("accessibility: invalid list position".into());
        }
        depths.insert(node.id.clone(), depth);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AccessibilityRole;
    fn node(id: &str, parent: Option<&str>) -> AccessibleNode {
        AccessibleNode {
            id: id.into(),
            parent: parent.map(str::to_owned),
            role: AccessibilityRole::Group,
            label: String::new(),
            description: None,
            value: None,
            col: 0,
            row: 0,
            cols: 1,
            rows: 1,
            selected: None,
            expanded: None,
            disabled: false,
            focused: false,
            actions: vec![],
            position: None,
            set_size: None,
        }
    }
    #[test]
    fn malformed_trees_and_unbounded_metadata_are_rejected() {
        assert!(validate(&[node("a", None), node("b", Some("a"))]).is_ok());
        for nodes in
            [vec![node("a", Some("a"))], vec![node("a", None), node("a", None)], vec![node("a", Some("missing"))]]
        {
            assert!(validate(&nodes).is_err());
        }
        let mut n = node("a", None);
        n.label = "x".repeat(16385);
        assert!(validate(&[n]).is_err());
        let mut a = node("a", None);
        a.focused = true;
        let mut b = node("b", None);
        b.focused = true;
        assert!(validate(&[a.clone(), b]).is_err());
        a.disabled = true;
        assert!(validate(&[a]).is_err());
        let nodes = (0..66)
            .map(|i| {
                let mut n = node(&i.to_string(), None);
                n.parent = (i > 0).then(|| (i - 1).to_string());
                n
            })
            .collect::<Vec<_>>();
        assert!(validate(&nodes).is_err());
    }
}
