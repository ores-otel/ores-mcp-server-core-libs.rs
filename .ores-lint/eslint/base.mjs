/**
 * ores-lint :: shared flat-config factory
 *
 * Everything here degrades gracefully. A repo missing TypeScript tooling gets
 * its JS linted rather than an error; an ESLint that has dropped core `semi`
 * falls back to the vendored rule. The point is that a lint config rolled out
 * to hundreds of heterogeneous repos must never be the thing that breaks them.
 */

import { createRequire } from 'node:module';
import oresPlugin from './plugin.mjs';

const require_ = createRequire(import.meta.url);

function tryResolve(id) {
  try { return require_.resolve(id); } catch { return null; }
}

/** Is core `semi` still shipped by the installed ESLint? */
async function coreSemiAvailable() {
  try {
    const { builtinRules } = await import('eslint/use-at-your-own-risk');
    return builtinRules.has('semi');
  } catch {
    return true; // unsupported API missing: assume core rules are intact
  }
}

/** typescript-eslint, if the repo happens to have it. */
async function loadTsSupport() {
  for (const id of ['typescript-eslint', '@typescript-eslint/parser']) {
    if (!tryResolve(id)) continue;
    try {
      const mod = await import(id);
      const m = mod.default || mod;
      if (id === 'typescript-eslint' && m.parser) return { parser: m.parser, source: id };
      if (m.parseForESLint || m.parse) return { parser: m, source: id };
    } catch { /* fall through to the next candidate */ }
  }
  return null;
}

const JS_FILES = ['**/*.js', '**/*.mjs', '**/*.cjs', '**/*.jsx'];
const TS_FILES = ['**/*.ts', '**/*.mts', '**/*.cts', '**/*.tsx'];

const IGNORES = [
  '**/node_modules/**', '**/dist/**', '**/build/**', '**/out/**', '**/target/**',
  '**/coverage/**', '**/.next/**', '**/vendor/**', '**/*.min.js', '**/*.bundle.js',
  '**/.ores-lint/**',
];

/**
 * @param {object} [opts]
 * @param {object} [opts.requireSend]  options forwarded to ores/require-send
 * @param {object} [opts.rules]        extra rules merged last (repo overrides)
 * @param {string[]} [opts.ignores]    extra ignore globs
 */
export default async function oresConfig(opts = {}) {
  const useCoreSemi = await coreSemiAvailable();
  const ts = await loadTsSupport();

  const semiRules = useCoreSemi
    ? { semi: ['warn', 'always'], 'no-extra-semi': 'warn', 'semi-style': ['warn', 'last'] }
    : { 'ores/semi': 'warn' };

  // Correctness rules chosen for a near-zero false-positive rate, because this
  // config lands in repos nobody is going to hand-tune afterwards.
  const correctness = {
    'ores/require-send': ['warn', opts.requireSend || {}],
    'no-unused-vars': ['warn', { args: 'after-used', argsIgnorePattern: '^_', varsIgnorePattern: '^_', caughtErrors: 'none' }],
    eqeqeq: ['warn', 'smart'],
    'no-fallthrough': 'warn',
    'no-unreachable': 'warn',
    'no-dupe-keys': 'warn',
    'no-dupe-else-if': 'warn',
    'no-duplicate-case': 'warn',
    'no-self-compare': 'warn',
    'no-unsafe-negation': 'warn',
    'no-cond-assign': ['warn', 'always'],
    'no-constant-condition': ['warn', { checkLoops: false }],
    'no-async-promise-executor': 'warn',
    'no-promise-executor-return': 'warn',
    'no-compare-neg-zero': 'warn',
    'no-irregular-whitespace': 'warn',
    'no-template-curly-in-string': 'warn',
    'valid-typeof': 'warn',
    'use-isnan': 'warn',
    'no-debugger': 'warn',
    // Deliberately NOT enabled: no-undef. Without a `globals` package it fires
    // on console/process/window everywhere, and TypeScript already covers it.
  };

  const configs = [
    { ignores: [...IGNORES, ...(opts.ignores || [])] },
    {
      files: JS_FILES,
      plugins: { ores: oresPlugin },
      languageOptions: { ecmaVersion: 'latest', sourceType: 'module' },
      rules: { ...semiRules, ...correctness, ...(opts.rules || {}) },
    },
  ];

  if (ts) {
    configs.push({
      files: TS_FILES,
      plugins: { ores: oresPlugin },
      languageOptions: { parser: ts.parser, ecmaVersion: 'latest', sourceType: 'module' },
      rules: {
        ...semiRules,
        ...correctness,
        // TypeScript's own compiler reports unused symbols with better fidelity.
        'no-unused-vars': 'off',
        ...(opts.rules || {}),
      },
    });
  }

  return configs;
}

export const meta = { tsSupport: null };
