// Minimal DOM shim to execute the REAL generated portal.html inline script
// and drive the try-it interactions headlessly. No deps, no installs.
const fs = require('fs');
const html = fs.readFileSync(process.argv[2], 'utf-8');

function makeClassList(el) {
  return {
    add(c) { el._cls.add(c); }, remove(c) { el._cls.delete(c); },
    toggle(c, f) { if (f === undefined) f = !el._cls.has(c); f ? el._cls.add(c) : el._cls.delete(c); return f; },
    contains(c) { return el._cls.has(c); },
  };
}
function makeEl(tag, attrs, parent) {
  const el = {
    tag, attrs, parent, children: [], _cls: new Set((attrs.class || '').split(/\s+/).filter(Boolean)),
    style: {}, value: attrs.value || '', textContent: '', _inner: '',
    listeners: {},
    get classList() { return makeClassList(el); },
    getAttribute(k) { return Object.prototype.hasOwnProperty.call(el.attrs, k) ? el.attrs[k] : null; },
    setAttribute(k, v) { el.attrs[k] = String(v); },
    removeAttribute(k) { delete el.attrs[k]; },
    hasAttribute(k) { return Object.prototype.hasOwnProperty.call(el.attrs, k); },
    addEventListener(t, f) { (el.listeners[t] = el.listeners[t] || []).push(f); },
    click() { (el.listeners.click || []).forEach(f => f.call(el)); },
    input() { (el.listeners.input || []).forEach(f => f.call(el)); },
    querySelector(s) { return queryAll(s, el)[0] || null; },
    querySelectorAll(s) { return queryAll(s, el); },
    appendChild(c) { el.children.push(c); c.parent = el; if (el.tag === 'select' && el.value === '' && c.value !== undefined) el.value = c.value; return c; },
    removeChild(c) { el.children = el.children.filter(x => x !== c); return c; },
    getBoundingClientRect() { return { top: 9999 }; },
    select() {}, scrollIntoView() {},
  };
  Object.defineProperty(el, 'innerHTML', { get() { return el._inner; }, set(v) { el._inner = String(v); el.textContent = el._inner.replace(/<[^>]*>/g, ''); } });
  return el;
}

// --- tiny parser: flat list + parent links ---
const VOID = new Set(['input', 'br', 'hr', 'img', 'meta', 'link']);
const root = makeEl('root', {}, null);
const all = [root];
let stack = [root];
const re = /<(\/?)([a-zA-Z0-9]+)([^<>]*)>/g;
let m;
while ((m = re.exec(html))) {
  const closing = m[1] === '/', tag = m[2].toLowerCase(), rest = m[3];
  if (closing) { while (stack.length > 1 && stack[stack.length - 1].tag !== tag) stack.pop(); if (stack.length > 1) stack.pop(); continue; }
  const attrs = {};
  const are = /([a-zA-Z_:][\w:.-]*)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+)))?/g;
  let a;
  while ((a = are.exec(rest))) attrs[a[1]] = a[2] !== undefined ? a[2] : (a[3] !== undefined ? a[3] : (a[4] !== undefined ? a[4] : ''));
  const el = makeEl(tag, attrs, stack[stack.length - 1]);
  stack[stack.length - 1].children.push(el);
  all.push(el);
  if (!VOID.has(tag) && !rest.trim().endsWith('/')) stack.push(el);
}
function descendants(n) { const out = []; (function w(x) { x.children.forEach(c => { out.push(c); w(c); }); })(n); return out; }
function match(el, sel) {
  let s = sel.trim(), tag = null, id = null;
  const tm = s.match(/^([a-zA-Z][\w-]*)/); if (tm) { tag = tm[1]; s = s.slice(tag.length); }
  const im = s.match(/#([\w-]+)/); if (im) { id = im[1]; s = s.replace('#' + id, ''); }
  const classes = [...s.matchAll(/\.([\w-]+)/g)].map(x => x[1]);
  const am = s.match(/\[([\w-]+)(?:=(?:'([^']*)'|"([^"]*)"|([^\]]+)))?\]/);
  if (tag && el.tag !== tag) return false;
  if (id && el.attrs.id !== id) return false;
  if (!classes.every(c => el._cls.has(c))) return false;
  if (am) {
    const v = am[2] !== undefined ? am[2] : (am[3] !== undefined ? am[3] : am[4]);
    if (!Object.prototype.hasOwnProperty.call(el.attrs, am[1])) return false;
    if (v !== undefined && el.attrs[am[1]] !== v) return false;
  }
  return true;
}
function queryAll(sel, scope) {
  const parts = sel.trim().split(/\s+/);
  let pool = parts.length === 1 ? descendants(scope || root) : null;
  if (parts.length > 1) { // single-level descendant only (enough for this page)
    pool = [];
    descendants(scope || root).forEach(x => { if (match(x, parts[0])) pool.push(...descendants(x)); });
    const last = parts[parts.length - 1];
    return pool.filter(x => match(x, last));
  }
  return pool.filter(x => match(x, parts[0]));
}
const document = {
  querySelector(s) { return queryAll(s)[0] || null; },
  querySelectorAll(s) { return queryAll(s); },
  createElement(t) { return makeEl(t, {}, null); },
  body: null, listeners: {},
  addEventListener(t, f) { (document.listeners[t] = document.listeners[t] || []).push(f); },
  execCommand() { return true; },
};
document.body = makeEl('body', {}, root);
global.document = document;
global.navigator = {};
global.window = global;

// --- fetch stub (harness-controlled) ---
let nextResp = { status: 200, body: '{}' };
global.fetch = () => Promise.resolve({ status: nextResp.status, text: () => Promise.resolve(nextResp.body) });

// --- run the REAL page script ---
let js = html.match(/<script>([\s\S]*)<\/script>/)[1];
eval(js);

// --- helpers ---
let pass = 0, fail = 0;
function vis(el) { return el && !el.hasAttribute('hidden') && !el.classList.contains('hidden'); }
function check(name, cond, extra) {
  if (cond) { pass++; console.log('PASS ' + name); }
  else { fail++; console.log('FAIL ' + name + (extra ? ' :: ' + extra : '')); }
}
const tick = () => new Promise(r => setTimeout(r, 20));

(async () => {
  const box = document.querySelector(".try[data-kind='post']");
  const area = box.querySelector('.try-body'), resp = box.querySelector('.try-resp'),
    send = box.querySelector('.send'), view = box.querySelector('.try-view'),
    copy = box.querySelector('.copybtn');
  const modes = {}; box.querySelectorAll('.mode').forEach(b => { modes[b.getAttribute('data-mode')] = b; });
  box.querySelector('.try-server').value = 'https://dev.dex-api.biya.io';

  // A: 400 error body must become visible, pretty, no scroll hunting
  area.value = '{"type":"nope"}';
  nextResp = { status: 400, body: '{"error":"invalid_or_unknown_info_type"}' };
  send.click(); await tick();
  check('A1 resp visible after send', vis(resp), 'hidden attr=' + resp.hasAttribute('hidden') + ' hidden class=' + resp.classList.contains('hidden'));
  check('A2 resp shows error content', resp.textContent.includes('invalid_or_unknown_info_type'), JSON.stringify(resp.textContent.slice(0, 80)));
  check('A3 resp pretty-printed', resp.textContent.includes('\n  "error"') || resp.textContent.includes('\n'), JSON.stringify(resp.textContent.slice(0, 60)));

  // B: 200 path
  nextResp = { status: 200, body: '{"a":1}' };
  send.click(); await tick();
  check('B resp 200 pretty', vis(resp) && resp.textContent.includes('"a": 1'), resp.textContent.slice(0, 60));

  // C: bad JSON input
  area.value = '{oops';
  send.click(); await tick();
  check('C bad-json error visible', vis(resp) && /解析失败/.test(resp.textContent), resp.textContent.slice(0, 60));

  // D: mode tabs
  area.value = '{"type":"meta"}';
  modes.curl.click();
  check('D1 curl: area hidden, view+copy shown', !vis(area) && vis(view) && vis(copy));
  check('D2 curl content follows', view.textContent.includes('curl') && view.textContent.includes('"type": "meta"') || view.textContent.includes('meta'), view.textContent.slice(0, 60));
  modes.json.click();
  check('D3 json: area shown, view+copy hidden', vis(area) && !vis(view) && !vis(copy));

  console.log(`\nresult: pass=${pass} fail=${fail}`);
  process.exit(fail ? 1 : 0);
})().catch(e => { console.error('HARNESS ERROR', e); process.exit(2); });
