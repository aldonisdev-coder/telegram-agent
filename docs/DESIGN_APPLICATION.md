# Design System Application Plan — Web Dashboard

Chief Design Officer (L6) review. Source of truth: `design-system/telegram-agent/MASTER.md`.
Target: server-rendered UI in `src/web.rs` (`html_page()`), single-binary, no build step.

## 1. Strategy
- Keep everything inline in `html_page()` (CSS + tiny JS). No external build, no asset pipeline.
- Fonts: Fira Code (headings) + Fira Sans (body) via Google Fonts `<link>` with system fallbacks (works offline via fallbacks).
- Theme: light default, dark via `data-theme` on `<html>`, persisted in `localStorage`, auto from `prefers-color-scheme`.
- Reuse existing class names (`.card`, `.btn`, `input`) plus new BEM-ish utility classes; base element styles so unclassed markup still looks right.

## 2. Color tokens

Light (default, `:root`):
| Token | Value |
|---|---|
| --color-primary | #0F172A |
| --color-on-primary | #FFFFFF |
| --color-secondary | #1E293B |
| --color-on-secondary | #FFFFFF |
| --color-accent | #16A34A |
| --color-on-accent | #0F172A |
| --color-background | #F8FAFC |
| --color-foreground | #0F172A |
| --color-card | #FFFFFF |
| --color-card-foreground | #0F172A |
| --color-muted | #E2E8F0 |
| --color-muted-foreground | #475569 |
| --color-border | #CBD5E1 |
| --color-destructive | #DC2626 |
| --color-on-destructive | #FFFFFF |
| --color-ring | #16A34A |
| --color-success | #16A34A |
| --color-warning | #D97706 |

Dark (`[data-theme="dark"]`), from MASTER.md OLED palette:
| Token | Value |
|---|---|
| --color-background | #020617 |
| --color-foreground | #F8FAFC |
| --color-card | #0E1223 |
| --color-card-foreground | #F8FAFC |
| --color-muted | #1A1E2F |
| --color-muted-foreground | #94A3B8 |
| --color-border | #334155 |
| --color-ring | #FFFFFF |
| --color-warning | #F59E0B |
(other roles identical)

## 3. Typography
- Heading font: `Fira Code, "SF Mono", ui-monospace, monospace` (`--font-heading`).
- Body font: `Fira Sans, -apple-system, BlinkMacSystemFont, "SF Pro Text", "Helvetica Neue", sans-serif` (`--font-body`).
- Scale: H1 28/600/-0.02em, H2 22/600/-0.01em, H3 18/600, body 15/400, small 13/400, caption 11/500 uppercase tracked 0.05em.
- Weights 400/500/600 only. Max line length 65ch.

## 4. Spacing
`--space-xs 4px`, `sm 8px`, `md 16px`, `lg 24px`, `xl 32px`, `2xl 48px`, `3xl 64px` (8px grid).

## 5. Radius & shadows
- Radius: `--radius-sm 8px` (buttons/inputs), `--radius-md 12px` (cards), `--radius-lg 16px` (modals).
- Shadows: `--shadow-sm/md/lg/xl` per MASTER.md.

## 6. Component class names
- Layout: `.container`, `.nav`, `.nav a`, `.form-row`, `.form-group`, `.table-wrap`.
- Surfaces: `.card`, `.card:hover`, `.modal-overlay`, `.modal`, `article`.
- Buttons: `button` (base), `.btn`, `.btn-secondary`, `.btn-danger`, `.btn-ghost`, `.btn-sm`.
- Forms: `.input`, `input/textarea/select` (base), `label`, `.form-group`.
- Data: `table`, `th`, `td`, `tr:hover`, `.badge`, `.badge-success`, `.badge-danger`, `.badge-warning`.
- States: `.empty`, `.sr-only`.

## 7. Responsive breakpoints
- `<=768px`: tighter padding, smaller H1, card padding md.
- `<=480px`: minimal padding.
- `prefers-reduced-motion`: disable transitions/animations.
- Targets verified: 375, 768, 1024, 1440px.

## 8. Dark mode toggle
- Toggle button in `.nav`: `<button class="btn btn-ghost btn-sm theme-toggle" onclick="toggleTheme()">Theme</button>`.
- Inline script sets `data-theme` from `localStorage` or `prefers-color-scheme` before paint; `toggleTheme()` flips and persists.

## 9. Component markup patterns

Card:
```html
<article class="card">
  <h3>Title</h3>
  <p>Supporting text</p>
</article>
```

Button row:
```html
<button type="submit" class="btn">Primary</button>
<button type="button" class="btn btn-secondary">Secondary</button>
<button type="button" class="btn btn-danger">Delete</button>
<button type="button" class="btn btn-ghost btn-sm">Cancel</button>
```

Form:
```html
<form method="post" action="/plan">
  <div class="form-group">
    <label>Name<input class="input" name="name" required></label>
  </div>
  <div class="form-group">
    <label>Price<input class="input" name="price" required></label>
  </div>
  <button type="submit" class="btn">Create plan</button>
</form>
```

Table:
```html
<div class="table-wrap">
  <table>
    <thead><tr><th>Name</th><th>Price</th></tr></thead>
    <tbody><tr><td>Pro</td><td>10 USD</td></tr></tbody>
  </table>
</div>
```

Empty state:
```html
<p class="empty">No plans yet. Create one above.</p>
```

Badge:
```html
<span class="badge badge-success">active</span>
<span class="badge badge-danger">failed</span>
<span class="badge badge-warning">queued</span>
```

Modal:
```html
<div class="modal-overlay">
  <div class="modal">
    <h2>Confirm delete</h2>
    <p>This cannot be undone.</p>
    <button class="btn btn-danger">Delete</button>
    <button class="btn btn-ghost">Cancel</button>
  </div>
</div>
```

Nav:
```html
<nav class="nav">
  <a href="/dashboard">Dashboard</a>
  <a href="/channels">Channels</a>
  <a href="/plans">Plans</a>
  <a href="/posts">Posts</a>
  <a href="/logout">Logout</a>
  <button type="button" class="btn btn-ghost btn-sm theme-toggle" onclick="toggleTheme()">Theme</button>
</nav>
```

## 10. Implementation status
CSS variables + component styles + dark mode toggle + responsive breakpoints implemented inline in `html_page()` in `src/web.rs`. Duplicate per-page nav removed (global nav now in `html_page()`). Tests passing.
