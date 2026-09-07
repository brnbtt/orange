import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { runInNewContext } from 'node:vm';
import test from 'node:test';

function keys(value, prefix = '') {
  if (!value || typeof value !== 'object') return [prefix];
  return Object.entries(value).flatMap(([key, child]) =>
    keys(child, prefix ? `${prefix}.${key}` : key));
}

async function loadI18n({ language = 'en-US', storage = {}, heading = null } = {}) {
  const applied = [];
  const elements = {
    '[data-i18n]': [{ dataset: { i18n: 'nav.download' }, textContent: 'Download' }],
    '[data-i18n-html]': [],
    '[data-i18n-alt]': [],
    '[data-i18n-aria]': [],
    '[data-set-lang]': [
      { dataset: { setLang: 'en' }, setAttribute() {}, addEventListener() {} },
      { dataset: { setLang: 'pt-BR' }, setAttribute() {}, addEventListener() {} },
    ],
    '[data-i18n="notFound.heading"]': heading ? [{ dataset: { i18n: 'notFound.heading' } }] : [],
  };
  const document = {
    documentElement: { lang: 'en' },
    title: '',
    querySelector: (selector) => selector === 'meta[name="description"]' ? { content: '' }
      : selector === 'meta[property="og:title"]' ? { content: '' }
      : selector === 'meta[property="og:description"]' ? { content: '' }
      : heading && selector === '[data-i18n="notFound.heading"]' ? elements[selector][0]
      : null,
    querySelectorAll: (selector) => elements[selector] || [],
  };
  const source = await readFile(new URL('./i18n.js', import.meta.url), 'utf8');
  const sandbox = {
    window: {},
    document,
    navigator: { language },
    localStorage: {
      getItem: (key) => storage[key] ?? null,
      setItem: (key, value) => { storage[key] = value; },
    },
  };
  sandbox.window = sandbox;
  runInNewContext(source, sandbox);
  applied.push(sandbox.window.orangeLocale);
  return { sandbox, document, storage, nav: elements['[data-i18n]'][0] };
}

test('both catalogs share the same keys', async () => {
  const { sandbox } = await loadI18n();
  const { en, 'pt-BR': pt } = sandbox.window.orangeI18n.catalogs;
  assert.deepEqual(keys(en).sort(), keys(pt).sort());
});

test('Portuguese Windows language selects pt-BR without a saved preference', async () => {
  const { sandbox, document, nav } = await loadI18n({ language: 'pt-BR' });
  assert.equal(sandbox.window.orangeLocale, 'pt-BR');
  assert.equal(document.documentElement.lang, 'pt-BR');
  assert.equal(nav.textContent, 'Baixar');
});

test('a saved English preference wins over the browser language', async () => {
  const { sandbox } = await loadI18n({ language: 'pt-BR', storage: { 'orange-lang': 'en' } });
  assert.equal(sandbox.window.orangeLocale, 'en');
});

test('the 404 page keeps its own title', async () => {
  const { document } = await loadI18n({ language: 'pt-BR', heading: true });
  assert.equal(document.title, 'Página não encontrada — Orange');
});
