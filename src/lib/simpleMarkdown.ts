/**
 * Lightweight markdown-to-HTML for strategy text and analysis summaries.
 * Supports: headings (# ~ ####), bullet lists (- / *), numbered lists,
 * bold (**), inline code (`), and paragraphs. No external dependencies.
 */
export function renderMarkdown(src: string): string {
  const lines = src.split("\n");
  const out: string[] = [];
  let inList: "ul" | "ol" | null = null;

  const closeList = () => {
    if (inList) {
      out.push(inList === "ul" ? "</ul>" : "</ol>");
      inList = null;
    }
  };

  for (const raw of lines) {
    const line = raw.trimEnd();

    // blank line → close list + paragraph break
    if (!line.trim()) {
      closeList();
      out.push("");
      continue;
    }

    // headings
    const hMatch = line.match(/^(#{1,4})\s+(.+)/);
    if (hMatch) {
      closeList();
      const level = hMatch[1].length;
      out.push(`<h${level + 1}>${inlineFormat(hMatch[2])}</h${level + 1}>`);
      continue;
    }

    // unordered list
    const ulMatch = line.match(/^\s*[-*]\s+(.+)/);
    if (ulMatch) {
      if (inList !== "ul") {
        closeList();
        out.push("<ul>");
        inList = "ul";
      }
      out.push(`<li>${inlineFormat(ulMatch[1])}</li>`);
      continue;
    }

    // ordered list
    const olMatch = line.match(/^\s*\d+[.)]\s+(.+)/);
    if (olMatch) {
      if (inList !== "ol") {
        closeList();
        out.push("<ol>");
        inList = "ol";
      }
      out.push(`<li>${inlineFormat(olMatch[1])}</li>`);
      continue;
    }

    // plain paragraph line
    closeList();
    out.push(`<p>${inlineFormat(line)}</p>`);
  }
  closeList();

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
