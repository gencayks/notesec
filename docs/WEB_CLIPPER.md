# Web clipper

NoteSec can receive pages from your browser: a bookmarklet (or a small
extension) sends the page's title, address and HTML to NoteSec, which
saves it as a new page. Design notes: decision 51 in `ARCHITECTURE.md`;
code in `src/clipper/` and `src/app/clipper_ui.rs`.

It is **off by default**. Turn it on in Settings → Web clipper. The
listener runs only while it is on and NoteSec is open, and only on
`127.0.0.1` (never `0.0.0.0` or another interface).

## Settings

| Setting | Where | Notes |
|---|---|---|
| On / Off | `config.toml`: `web_clipper` | Starts or stops the listener at once. |
| Port | `config.toml`: `clipper_port` | Default `27183`. Settings has −, +, Default; any port 1024–65535 can be set in the file. A port in use is shown in Settings and the status, and nothing else happens. |
| Token | `state.toml`: `clipper_token` | 64 hex digits (244 random bits), made the first time the clipper is turned on. Settings shows it masked, with Copy and Regenerate. Regenerating makes the old token (and bookmarklets with it) stop working immediately. |

"Copy bookmarklet" puts a ready `javascript:` URL with the port and token
on the clipboard: create a bookmark and paste it as the bookmark's
address.

## What gets saved

A new page named after the page's title (cleaned: one line, `[` `]`
become `(` `)`, `/` becomes `-`, at most 150 bytes; "Title (2)" if the
name is taken; the site's host name if there's no title):

```markdown
- source:: https://example.com/article
  clipped:: [[2026-10-09]]
  tags:: #clipped
- # Heading
  - A paragraph with **bold**, *italics*, `code` and [a link](https://example.com/x).
  - [Image: diagram](https://example.com/diagram.png)
```

and `Clipped [[Title]]` at the end of today's journal (the inbox). The
status shows "Clipped “Title”" with an Open button.

Conversion keeps headings, paragraphs, lists (nested), block quotes,
code blocks, tables (one block per row, cells separated by `·`), links
(only `http:`, `https:`, `mailto:`; relative links are made absolute),
bold, italics and inline code. It drops scripts, styles, frames, forms,
buttons, embedded objects, SVG, canvas, video, audio, templates and
`<nav>` with everything inside them, and every attribute except `href`
and `src` (so no `onclick` or `style`). Images are **not downloaded**:
they become links. Text that would mean something in our markdown
(`[[links]]`, `((block refs))`, `{{macros}}`, a line like `key:: value`
or `- item` or `# heading`) gets an invisible zero-width space so it
stays plain text. The result is capped at 1 MB and 5,000 blocks (the
page then ends with a note that the clip was shortened).

## The request

```
POST /clip HTTP/1.1
Host: 127.0.0.1:27183
Content-Type: application/x-www-form-urlencoded
Content-Length: 123

token=…&title=…&url=…&html=…
```

| Part | Required | Notes |
|---|---|---|
| Method, path | yes | `POST /clip` (a query string is ignored). `OPTIONS /clip` answers a CORS preflight. Other methods: 405; other paths: 404. |
| `Host` | yes | Exactly `127.0.0.1:<port>` or `localhost:<port>`; anything else is 403 (DNS rebinding). |
| `Content-Type` | yes | `application/x-www-form-urlencoded` (form, the bookmarklet) or `application/json` (extensions). A `charset` parameter is allowed (UTF-8 is assumed). Others: 415. |
| `Content-Length` | yes | At most 5 MB (5,242,880 bytes). Missing: 411; too large: 413. |
| `Transfer-Encoding` | no | Refused (501), chunked included. |
| `Expect: 100-continue` | no | Answered with `100 Continue`. |
| `X-NoteSec-Token` | no | The token, instead of the `token` field. A wrong one is refused before the body is read. |

Fields (form fields or JSON string members; other members are ignored):

| Field | Notes |
|---|---|
| `token` | Required unless the header carries it. Compared in constant time. |
| `title` | The page's title (at most 4,096 bytes). |
| `url` | The page's address (at most 4,096 bytes); kept as `source::` only if it is `http(s)`. Also used to make relative links absolute. |
| `html` | The HTML to clip: the whole page or a selection. |

At least one of `title`, `url`, `html` must be non-empty.

JSON example:

```json
{"token": "…", "title": "Example", "url": "https://example.com/", "html": "<p>Hello</p>"}
```

## Responses

Every response has `Connection: close`, `Cache-Control: no-store`,
`X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`. The
JSON and plain-text ones carry `Access-Control-Allow-Origin: *` (never
`Access-Control-Allow-Credentials`: the token is the only credential);
the HTML pages answering a form post (a navigation, not a script's
request) don't need it.

| Status | When | Body |
|---|---|---|
| 200 | Saved | Form: a small HTML page "Saved to NoteSec ✓" with the title (escaped) that closes its window after 1.5 s. JSON: `{"ok":true,"title":"<page title>"}` (the title it was saved under). |
| 204 | `OPTIONS` preflight | `Access-Control-Allow-Methods: POST, OPTIONS`, `Access-Control-Allow-Headers: Content-Type, X-NoteSec-Token`, `Access-Control-Max-Age: 600`, and `Access-Control-Allow-Private-Network: true` when the preflight asks for it. |
| 400 | Malformed request line, header, body, JSON; nothing to clip; a field too long | |
| 403 | Wrong `Host`; wrong or missing token | |
| 404 | Another path | |
| 405 | Another method (`Allow: POST, OPTIONS`) | |
| 408 | The request took more than 15 s to arrive | |
| 411 | No `Content-Length` | |
| 413 | Body over 5 MB | |
| 415 | Another `Content-Type` | |
| 429 | More than 30 clips in a minute (`Retry-After: 60`) | |
| 431 | Request head over 16 KB or 64 headers | |
| 500 | NoteSec couldn't save the page | |
| 501 | `Transfer-Encoding` | |
| 503 | More than 4 connections at once (`Retry-After: 2`); NoteSec didn't answer within 10 s or is closing | |
| 505 | HTTP version other than 1.0 / 1.1 | |

Error bodies: before the body type is known (Host, path, method, limits),
`text/plain` like `403 Forbidden: unknown Host: …`. After it: for a form
an HTML page "Not saved" with the reason, for JSON
`{"ok":false,"error":"<reason>"}`. The HTML pages are self-contained,
send `Content-Security-Policy: default-src 'none'; style-src 'nonce-…';
script-src 'nonce-…'; base-uri 'none'; form-action 'none';
frame-ancestors 'none'` and `X-Frame-Options: DENY`, and contain no
unescaped input.

A 503 "didn't answer in time" can come after NoteSec saved the page
(it was busy): check before sending again.

## The bookmarklet

The readable source of what "Copy bookmarklet" produces (`PORT` and
`TOKEN` filled in):

```js
javascript:(function () {
  var s = getSelection(), h = '', d, i, f, k, e, w = 'notesec' + Date.now();
  if (s && s.rangeCount && !s.isCollapsed) {
    // A selection: only that.
    d = document.createElement('div');
    for (i = 0; i < s.rangeCount; i++) d.appendChild(s.getRangeAt(i).cloneContents());
    h = d.innerHTML;
  } else {
    h = (document.querySelector('article') || document.querySelector('main') || document.body).innerHTML;
  }
  var v = { token: 'TOKEN', title: document.title, url: location.href, html: h.slice(0, 1500000) };
  // A small window for the answer, then a top-level form post into it.
  window.open('about:blank', w, 'width=420,height=260');
  f = document.createElement('form');
  f.method = 'post';
  f.action = 'http://127.0.0.1:PORT/clip';
  f.target = w;
  f.acceptCharset = 'utf-8';
  for (k in v) {
    e = document.createElement('input');
    e.type = 'hidden'; e.name = k; e.value = v[k];
    f.appendChild(e);
  }
  document.body.appendChild(f);
  f.submit();
  f.remove();
})()
```

It posts a form (a navigation) rather than calling `fetch()`: browsers
increasingly block a public site's scripts from reaching `localhost`
(Chrome's Private Network Access), and a page's `connect-src` CSP would
block `fetch` too. The HTML is cut at 1.5 million characters so the
encoded body stays under the 5 MB cap.

## A minimal extension

`manifest.json` (Chrome / Edge, Manifest V3):

```json
{
  "manifest_version": 3,
  "name": "Clip to NoteSec",
  "version": "1.0",
  "permissions": ["activeTab", "scripting", "storage"],
  "host_permissions": ["http://127.0.0.1/*"],
  "background": { "service_worker": "background.js" },
  "action": { "default_title": "Clip to NoteSec" }
}
```

`background.js` (store the token once, e.g. from the extension's
console: `chrome.storage.local.set({token: '…', port: 27183})`):

```js
chrome.action.onClicked.addListener(async (tab) => {
  const [{ result: page }] = await chrome.scripting.executeScript({
    target: { tabId: tab.id },
    func: () => {
      const s = getSelection();
      let html;
      if (s && s.rangeCount && !s.isCollapsed) {
        const d = document.createElement('div');
        for (let i = 0; i < s.rangeCount; i++) d.appendChild(s.getRangeAt(i).cloneContents());
        html = d.innerHTML;
      } else {
        html = (document.querySelector('article') || document.querySelector('main') || document.body).innerHTML;
      }
      return { title: document.title, url: location.href, html: html.slice(0, 3000000) };
    },
  });
  const { token, port = 27183 } = await chrome.storage.local.get(['token', 'port']);
  const response = await fetch(`http://127.0.0.1:${port}/clip`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', 'X-NoteSec-Token': token },
    body: JSON.stringify(page),
    credentials: 'omit',
  });
  const answer = await response.json();
  chrome.action.setBadgeText({ tabId: tab.id, text: answer.ok ? '✓' : '!' });
});
```

From a terminal:

```sh
curl -sS http://127.0.0.1:27183/clip -H "X-NoteSec-Token: $TOKEN" \
  -H 'Content-Type: application/json' \
  --data '{"title":"From curl","url":"https://example.com/","html":"<p>hi</p>"}'
```

## Threat model

Any website you visit can make your browser send requests to
`127.0.0.1`. What stops it from writing into your notes:

- **The token.** Every POST must carry it; without it the answer is 403
  and nothing is read further or written. A site can't read the token:
  it lives in NoteSec's `state.toml` and in your bookmarklet, and the
  clipper never sends it anywhere. It has 244 random bits and is
  compared in constant time, and failures are cheap to refuse. The
  responses never reflect the token.
- **The Host check** stops DNS rebinding: a page on `evil.example`
  whose name is made to resolve to `127.0.0.1` still sends
  `Host: evil.example:<port>`, which is refused, so it can't use
  same-origin reads to probe the endpoint.
- **CORS without credentials.** `Access-Control-Allow-Origin: *` lets an
  extension or a page that *has* the token use `fetch`; it gives
  nothing to one that doesn't (there are no cookies or other ambient
  credentials, and the responses contain nothing secret).
- **What a clip can do** once accepted is limited: it can only add a new
  page (never change or overwrite one) and a line in today's journal.
  Its HTML is converted to plain outline text: nothing is executed or
  downloaded, links can only be http/https/mailto, and our own syntax
  (`[[`, `((`, `{{`, properties, `id::`) is neutralised, so a page
  can't embed or reference your private pages or take over block ids.
- **Limits** keep a misbehaving client from tying NoteSec up: 4
  connections, 16 KB heads, 5 MB bodies, 15 s per request, 30 clips a
  minute, 1 MB of output per clip.

What it doesn't protect against:

- **Anything that has the token.** The bookmarklet contains it in plain
  text, so it is wherever your bookmarks are (including browser sync).
  If it leaks, press Regenerate (and copy the bookmarklet again).
- **Local programs and other users of this computer.** Anything that
  can connect to `127.0.0.1` can try; the token is what stops it. Malware
  running as you can read `state.toml` anyway.
- **`state.toml` holds the token**, like `ai_api_key`. Publishing never
  reads `state.toml`; any future sync or export of the graph folder must
  leave out both `clipper_token` and `ai_api_key`. Git backup (decision
  39) commits the graph folder, `state.toml` included, to a local
  repository and never pushes; if you push that repository yourself, the
  token is in its history (Regenerate makes the old one useless).
- **Plain HTTP on the loopback interface.** Traffic doesn't leave the
  machine, so there is no TLS.

## Known limits

- Sites whose Content-Security-Policy restricts `form-action` (GitHub,
  for one) block the bookmarklet's form; use the extension there.
- Browsers may start asking for permission (or block) before a public
  page navigates to a local address; the extension route is not
  affected.
- Pages that build their content with scripts after load are clipped as
  the browser shows them (the bookmarklet sends the live DOM), but
  content in iframes or shadow DOM is not included.
- Images are links, not copies; tables become one block per row.
- One clip at a time per connection, no keep-alive; nothing is queued
  while NoteSec is closed.
