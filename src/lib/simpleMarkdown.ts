/**
 * Lightweight markdown-to-HTML for strategy text and analysis summaries.
 * Supports: headings, bullet/numbered lists, tables, blockquotes,
 * bold, inline code, and paragraphs. No external dependencies.
 */
export function renderMarkdown(src: string): string {
  const lines = src.split("\n");
  const out: string[] = [];
  let inList: "ul" | "ol" | null = null;
  let inTable = false;
  let inBlockquote = false;

  const closeList = () => {
    if (inList) { out.push(inList === "ul" ? "</ul>" : "</ol>"); inList = null; }
  };
  const closeTable = () => {
    if (inTable) { out.push("</tbody></table>"); inTable = false; }
  };
  const closeBlockquote = () => {
    if (inBlockquote) { out.push("</blockquote>"); inBlockquote = false; }
  };
  const closeAll = () => { closeList(); closeTable(); closeBlockquote(); };

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i].trimEnd();

    // blank line
    if (!line.trim()) { closeAll(); out.push(""); continue; }

    // blockquote
    const bqMatch = line.match(/^>\s?(.*)/);
    if (bqMatch) {
      closeList(); closeTable();
      if (!inBlockquote) { out.push("<blockquote>"); inBlockquote = true; }
      out.push(`<p>${inlineFormat(bqMatch[1])}</p>`);
      continue;
    }
    if (inBlockquote && !bqMatch) closeBlockquote();

    // headings
    const hMatch = line.match(/^(#{1,4})\s+(.+)/);
    if (hMatch) {
      closeAll();
      const level = hMatch[1].length;
      out.push(`<h${level + 1}>${inlineFormat(hMatch[2])}</h${level + 1}>`);
      continue;
    }

    // table row
    if (line.includes("|") && line.trim().startsWith("|")) {
      closeList(); closeBlockquote();
      const cells = line.split("|").slice(1, -1).map(c => c.trim());
      // separator row (|---|---|)
      if (cells.every(c => /^[-:]+$/.test(c))) continue;
      if (!inTable) {
        out.push('<table><thead><tr>');
        cells.forEach(c => out.push(`<th>${inlineFormat(c)}</th>`));
        out.push('</tr></thead><tbody>');
        inTable = true;
      } else {
        out.push('<tr>');
        cells.forEach(c => out.push(`<td>${inlineFormat(c)}</td>`));
        out.push('</tr>');
      }
      continue;
    }
    if (inTable) closeTable();

    // unordered list
    const ulMatch = line.match(/^\s*[-*]\s+(.+)/);
    if (ulMatch) {
      closeTable(); closeBlockquote();
      if (inList !== "ul") { closeList(); out.push("<ul>"); inList = "ul"; }
      out.push(`<li>${inlineFormat(ulMatch[1])}</li>`);
      continue;
    }

    // ordered list
    const olMatch = line.match(/^\s*\d+[.)]\s+(.+)/);
    if (olMatch) {
      closeTable(); closeBlockquote();
      if (inList !== "ol") { closeList(); out.push("<ol>"); inList = "ol"; }
      out.push(`<li>${inlineFormat(olMatch[1])}</li>`);
      continue;
    }

    // plain paragraph
    closeAll();
    out.push(`<p>${inlineFormat(line)}</p>`);
  }
  closeAll();
  return out.join("\n");
}

function inlineFormat(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/\*\*(.+?)\*\*/g, "<strong>$1</strong>")
    .replace(/`(.+?)`/g, "<code>$1</code>");
}
