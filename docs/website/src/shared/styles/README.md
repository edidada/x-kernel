# Stylesheet boundaries

- `app.css` is the homepage stylesheet entry, imported by `main.tsx`.
- `theme.css` owns global font metrics, document colors, builder semantic tokens,
  and the shared theme/language navigation controls. Define each token here once.
- `home.css` owns homepage layout, navigation, footer and
  their responsive rules. Its dark rules change only those components.
- `builder.css` owns the `.new-exe-page` component. Every selector is scoped to it;
  it consumes tokens from `theme.css`, including error and input colors.
- `terminal.css` owns the terminal player and its theme variants.
- `../../assets/fonts/fonts.css` declares bundled font faces.

Theme selection has one input: `html[data-theme="light" | "dark"]`. `ThemeProvider` from `next-themes` sets it before first paint and `ThemeToggle` calls
its typed `useTheme` API to change it afterward. Component styles must not add independent system-color media
queries, which could override a user's explicit choice.

Keep layout rules with their component. Do not repair a component by appending
an overriding rule in a different stylesheet. In particular, the builder's
`.prompt-box` layout belongs in `builder.css`, not the homepage stylesheet.
