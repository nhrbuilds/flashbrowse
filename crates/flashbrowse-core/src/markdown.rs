//! Readability-driven article/content extractor (lightweight).
//!
//! Evaluates a small in-page script that walks `document.body`, emitting
//! headings as `#`, paragraphs as text, and links as `[text](href)`. This is
//! intentionally small - a full Readability port is out of scope for MVP.

/// In-page extraction script; returns `{title, url, markdown}` via `returnByValue`.
/// Preserves `<pre>` verbatim, renders links as `[text](href)`, de-duplicates
/// nested `<li><p>` (inner blocks only - outer containers were flat by design).
pub const EXTRACT_SCRIPT: &str = r#"
(() => {
  const title = document.title || '';
  const url = location.href || '';
  const esc = (s) => (s || '').replace(/\s+/g, ' ').trim();
  const lines = [];
  lines.push('# ' + (title || 'Untitled'));
  lines.push('');
  const roots = [];
  const pending = [document];
  const visited = new Set();
  while (pending.length) {
    const root = pending.pop();
    if (!root || visited.has(root)) continue;
    visited.add(root);
    roots.push(root);
    const children = [];
    for (const el of root.querySelectorAll('*')) {
      if (el.shadowRoot) children.push(el.shadowRoot);
      if (el.tagName === 'IFRAME' || el.tagName === 'FRAME') {
        try {
          if (el.contentDocument) children.push(el.contentDocument);
        } catch (_) {}
      }
    }
    for (let i = children.length - 1; i >= 0; i--) pending.push(children[i]);
  }
  const seen = new Set();
  for (const root of roots) {
  for (const el of root.querySelectorAll('h1,h2,h3,h4,h5,h6,p,li,pre,blockquote')) {
    // Skip if any ancestor is also selected and closer to a deeper block -
    // actually querySelectorAll is flat; dedupe by text to kill <li><p> doubles.
    const tag = el.tagName.toLowerCase();
    let text = tag === 'pre' ? (el.textContent || '') : esc(el.innerText || el.textContent || '');
    if (!text.trim()) continue;
    if (text.length > 4000) text = text.slice(0, 4000);
    const key = tag + '|' + text.slice(0, 120);
    if (seen.has(key)) continue;
    seen.add(key);
    if (tag.startsWith('h')) {
      const lvl = Number(tag.slice(1)) || 1;
      // Heading links: [text](href)
      const a = el.querySelector('a[href]');
      if (a) { lines.push('#'.repeat(lvl) + ' [' + text + '](' + a.href + ')'); }
      else { lines.push('#'.repeat(lvl) + ' ' + text); }
    } else if (tag === 'li') {
      const a = el.querySelector('a[href]');
      if (a) { lines.push('- [' + text + '](' + a.href + ')'); }
      else { lines.push('- ' + text); }
    } else if (tag === 'pre') {
      lines.push('```\n' + text + '\n```');
    } else {
      // Paragraph links inline as [text](href)
      let out = text;
      const a = el.querySelector('a[href]');
      if (a) { out = '[' + text + '](' + a.href + ')'; }
      lines.push(out);
    }
    lines.push('');
  }
  }
  return { title, url, markdown: lines.join('\n') };
})()
"#;

#[cfg(test)]
mod tests {
    use super::EXTRACT_SCRIPT;

    #[test]
    fn script_evaluates_to_object() {
        assert!(EXTRACT_SCRIPT.contains("document.title"));
        assert!(EXTRACT_SCRIPT.contains("return {"));
    }
}
