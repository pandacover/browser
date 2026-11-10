// Run: node tests/address.test.js
const assert = require("node:assert/strict");
const { addressToUrl, SEARCH_URL } = require("../ui/address.js");

const q = (s) => SEARCH_URL + encodeURIComponent(s);
const cases = [
  ["", null],
  ["   ", null],
  ["example.com", "https://example.com"],
  ["  example.com  ", "https://example.com"],
  ["github.com/rust-lang/rust", "https://github.com/rust-lang/rust"],
  ["sub.domain.co.uk/path?x=1#y", "https://sub.domain.co.uk/path?x=1#y"],
  ["example.com:8080", "https://example.com:8080"],
  ["example.com:8080/x", "https://example.com:8080/x"],
  ["https://example.com", "https://example.com"],
  ["HTTP://Example.com", "HTTP://Example.com"],
  ["about:blank", "about:blank"],
  ["edge://settings", "edge://settings"],
  ["file:///C:/Users/me/a.html", "file:///C:/Users/me/a.html"],
  ["mailto:someone@example.com", "mailto:someone@example.com"],
  ["localhost", "http://localhost"],
  ["localhost:3000", "http://localhost:3000"],
  ["localhost:3000/api?q=1", "http://localhost:3000/api?q=1"],
  ["127.0.0.1:8080", "http://127.0.0.1:8080"],
  ["192.168.1.1", "http://192.168.1.1"],
  ["[::1]:5173", "http://[::1]:5173"],
  ["myserver:9000", "https://myserver:9000"],
  ["rust", q("rust")],
  ["how to write rust", q("how to write rust")],
  ["what is 2+2?", q("what is 2+2?")],
  ["c++", q("c++")],
  ["user@example.com", q("user@example.com")],
  ["c:\\temp", q("c:\\temp")],
  ["javascript:alert(1)", q("javascript:alert(1)")],
  ["3.14", q("3.14")],
];

let failed = 0;
for (const [input, expected] of cases) {
  try {
    assert.equal(addressToUrl(input), expected);
  } catch {
    failed++;
    console.error(`FAIL ${JSON.stringify(input)}: got ${JSON.stringify(addressToUrl(input))}, want ${JSON.stringify(expected)}`);
  }
}
console.log(`${cases.length - failed}/${cases.length} address cases passed`);
process.exit(failed ? 1 : 0);
