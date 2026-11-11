/** Tiny glob matcher for the prune deny-list: `**` crosses directories,
 * `*` and `?` stay inside one. Paths are `/`-separated relatives.
 */

export function matchGlob(pattern: string, path: string): boolean {
  return globRe(pattern).test(path);
}

const cache = new Map<string, RegExp>();

function globRe(pattern: string): RegExp {
  let re = cache.get(pattern);
  if (re) return re;
  let src = "^";
  for (let i = 0; i < pattern.length; i++) {
    const c = pattern[i];
    if (c === "*") {
      if (pattern[i + 1] === "*" && (pattern[i + 2] === "/" || i + 2 === pattern.length)) {
        src += "(?:[^/]+/)*"; // **/ or trailing ** — any depth
        i += pattern[i + 2] === "/" ? 2 : 1;
      } else {
        src += "[^/]*";
      }
    } else if (c === "?") {
      src += "[^/]";
    } else {
      src += c.replace(/[.+^${}()|[\]\\]/g, "\\$&");
    }
  }
  // A pattern may match the path itself or any parent dir of it.
  re = new RegExp(`${src}(?:/.*)?$`);
  cache.set(pattern, re);
  return re;
}
