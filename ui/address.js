// Turns what the user typed in the address bar into a URL to load.
// No DOM access here, so it can be unit-tested with node: `node tests/address.test.js`.

const SEARCH_URL = "https://www.google.com/search?q=";
const KNOWN_SCHEMES = ["http", "https", "file", "about", "edge", "data", "blob", "view-source", "mailto", "ftp"];

function addressToUrl(input) {
  const text = input.trim();
  if (!text) return null;
  const search = () => SEARCH_URL + encodeURIComponent(text);

  // Anything with spaces is a search.
  if (/\s/.test(text)) return search();

  // localhost, IPv4 and bracketed IPv6, with optional port and path: plain http.
  if (/^(localhost|(\d{1,3}\.){3}\d{1,3}|\[[0-9a-f:]+\])(:\d+)?([/?#].*)?$/i.test(text)) {
    return "http://" + text;
  }

  // Explicit scheme (https://x, about:blank, mailto:x). "example.com:8080" is a port, not a scheme.
  const scheme = /^([a-z][a-z0-9+.-]*):(.*)$/i.exec(text);
  if (scheme) {
    const isPort = /^\d+([/?#]|$)/.test(scheme[2]);
    if (!isPort && (KNOWN_SCHEMES.includes(scheme[1].toLowerCase()) || scheme[2].startsWith("//"))) {
      return text;
    }
  }

  // host.tld[:port][/path] or host:port[/path]
  const path = "(:\\d+)?([/?#].*)?$";
  if (new RegExp("^([a-z0-9-]+\\.)+[a-z][a-z0-9-]+\\.?" + path, "i").test(text)) return "https://" + text;
  if (/^[a-z0-9-]+:\d+([/?#].*)?$/i.test(text)) return "https://" + text;

  return search();
}

if (typeof module !== "undefined") module.exports = { addressToUrl, SEARCH_URL };
