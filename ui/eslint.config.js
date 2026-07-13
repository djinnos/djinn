// For more info, see https://github.com/storybookjs/eslint-plugin-storybook#configuration-flat-config-format
import storybook from "eslint-plugin-storybook";

import js from '@eslint/js'
import globals from 'globals'
import reactHooks from 'eslint-plugin-react-hooks'
import reactRefresh from 'eslint-plugin-react-refresh'
import tseslint from 'typescript-eslint'
import { defineConfig, globalIgnores } from 'eslint/config'

export default defineConfig([
  // `src/api/generated` is machine-generated (ui/scripts/generate-usage-types.ts);
  // it carries its own blanket `/* eslint-disable */` header and must never be linted.
  // `*.config.d.ts` are tsc build artifacts emitted from the vite/vitest configs.
  globalIgnores([
    'dist',
    '.djinn',
    'src/api/generated',
    'vite.config.d.ts',
    'vitest.config.d.ts',
  ]),
  {
    files: ['**/*.{ts,tsx}'],
    extends: [
      js.configs.recommended,
      tseslint.configs.recommended,
      reactHooks.configs.flat.recommended,
      reactRefresh.configs.vite,
    ],
    languageOptions: {
      ecmaVersion: 2020,
      globals: globals.browser,
    },
    rules: {
      // Exporting non-component constants (e.g. cva variant maps) alongside a
      // component is idiomatic and does not break Fast Refresh.
      'react-refresh/only-export-components': [
        'error',
        { allowConstantExport: true },
      ],
      // Underscore-prefixed identifiers are the project's explicit "intentionally
      // unused" marker for args, destructured vars, and caught errors.
      '@typescript-eslint/no-unused-vars': [
        'error',
        {
          argsIgnorePattern: '^_',
          varsIgnorePattern: '^_',
          caughtErrorsIgnorePattern: '^_',
          destructuredArrayIgnorePattern: '^_',
        },
      ],
    },
  },
  {
    // Fast Refresh only matters for app source. Test and Storybook files
    // legitimately mix component and non-component exports.
    //
    // `src/components/ui` (shadcn/ui) and `src/components/ai-elements` (vendored
    // Vercel AI Elements) are design-system primitive modules that idiomatically
    // co-export cva variant maps and context hooks alongside their components.
    files: [
      '**/*.stories.{ts,tsx}',
      '**/*.test.{ts,tsx}',
      'src/test/**',
      'src/components/ui/**',
      'src/components/ai-elements/**',
    ],
    rules: {
      'react-refresh/only-export-components': 'off',
    },
  },
  {
    // Storybook CSF render/decorator args are loosely typed by design (Storybook's
    // own `args`/`StoryFn` surfaces are `any`); annotating each render callback
    // with a precise prop union adds noise without runtime value.
    files: ['**/*.stories.{ts,tsx}'],
    rules: {
      '@typescript-eslint/no-explicit-any': 'off',
    },
  },
])
