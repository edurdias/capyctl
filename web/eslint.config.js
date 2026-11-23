import svelte from 'eslint-plugin-svelte';
import tseslint from 'typescript-eslint';

export default tseslint.config(
  { ignores: ['dist/', 'node_modules/'] },
  ...tseslint.configs.recommended,
  ...svelte.configs['flat/recommended'],
  {
    files: ['**/*.svelte', '**/*.svelte.ts'],
    languageOptions: {
      parserOptions: {
        parser: tseslint.parser,
        extraFileExtensions: ['.svelte'],
      },
    },
  },
  {
    rules: {
      // ADR 0040 (plan KTD18): model output, engine logs and YAML render as
      // text, never as HTML; a script injected into the UI could add a custom
      // engine.
      'svelte/no-at-html-tags': 'error',
    },
  },
);
