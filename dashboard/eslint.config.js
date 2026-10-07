// Text from the API is other people's words (§8.3): it is shown as text,
// never as markup. These rules make the other way a lint error.
import js from '@eslint/js';
import svelte from 'eslint-plugin-svelte';
import ts from 'typescript-eslint';

const noMarkup = {
  'no-restricted-syntax': [
    'error',
    {
      selector: "AssignmentExpression > MemberExpression.left[property.name=/^(innerHTML|outerHTML)$/]",
      message: 'Show text as text: no innerHTML or outerHTML.',
    },
    {
      selector: "CallExpression[callee.property.name='insertAdjacentHTML']",
      message: 'Show text as text: no insertAdjacentHTML.',
    },
  ],
};

export default ts.config(
  { ignores: ['dist/', 'node_modules/'] },
  js.configs.recommended,
  ...ts.configs.strict,
  ...svelte.configs.recommended,
  {
    files: ['**/*.svelte', '**/*.svelte.ts'],
    languageOptions: { parserOptions: { parser: ts.parser } },
  },
  {
    rules: {
      ...noMarkup,
      'svelte/no-at-html-tags': 'error',
    },
  },
);
