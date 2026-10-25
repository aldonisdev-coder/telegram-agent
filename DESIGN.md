# Design System — Telegram Business Platform

## Perspectives
This design system is informed by two lenses:
- **Senior UX Designer**: usability, accessibility, interaction patterns, user research.
- **Chief Design Officer**: brand coherence, product strategy, design systems at scale.

---

## 1. Senior UX Designer View

### User research insights
- Sellers are often non-technical; setup must be < 5 minutes.
- Buyers want instant access after payment; friction kills conversion.
- Mobile-first: most Telegram users are on phones.

### Usability principles
- **One primary action per screen** (Fitts's Law).
- **Progressive disclosure**: show basics first, advanced on demand.
- **Forgiving**: confirm destructive actions; allow undo where possible.
- **Feedback**: every action gets immediate visual confirmation.
- **Empty states**: guide users when there's no data.

### Accessibility (WCAG 2.1 AA)
- Contrast ratio ≥ 4.5:1 for text.
- Keyboard navigable; visible focus rings.
- Semantic HTML (headings, landmarks, labels).
- Screen-reader friendly (aria labels).
- Reduced motion support (`prefers-reduced-motion`).

### Interaction patterns
- Forms: inline validation, clear errors.
- Lists: swipe actions on mobile, hover on desktop.
- Modals: only for destructive confirmations.
- Loading: skeleton screens, not spinners where possible.

---

## 2. Chief Design Officer View

### Brand identity
- **Name**: Telegram Business Platform.
- **Tone**: calm, confident, helpful — never loud.
- **Voice**: clear, concise, human. Avoid jargon.

### Design system at scale
- Single source of truth: tokens (color, spacing, type).
- Component library shared between bot (inline keyboards) and web.
- Theming: light (default), dark (future).

### Product strategy
- **North star**: time-to-first-sale < 10 minutes.
- **Activation**: guided onboarding wizard.
- **Retention**: analytics, scheduled posts, customer insights.
- **Growth**: referral codes, marketplace discovery.

### Design ops
- Design tokens in CSS variables.
- Component documentation in `DESIGN.md`.
- Review process: every UI change reviewed against principles.

---

## 3. Design Philosophy (Apple-inspired)
Clean, minimal, calm, and obvious. Generous whitespace, precise typography,
subtle depth, single clear action per screen.

## 4. Tokens

### Typography (Senior UX + CDO)
- **Font stack**: `-apple-system, BlinkMacSystemFont, "SF Pro Text", "SF Pro Display", "Helvetica Neue", "Segoe UI", Roboto, sans-serif`. System fonts only — no external font downloads (fast, private, native feel).
- **CDO rationale**: native system fonts build trust and reduce load time; aligns with Apple/Google/Microsoft platforms.
- **Senior UX rationale**: system fonts match user's OS, improving readability and accessibility; no FOUT/FOIT.
- **Type scale** (modular 1.25):
  - Display: 34px / 600 / 1.1
  - H1: 28px / 600 / 1.2
  - H2: 22px / 600 / 1.25
  - H3: 18px / 600 / 1.3
  - Body: 15px / 400 / 1.5
  - Small: 13px / 400 / 1.4
  - Caption: 11px / 500 / 1.3 (uppercase, tracked)
- **Weights**: 400 (regular), 500 (medium), 600 (semibold). Avoid 700+ for calm feel.
- **Line length**: max 65ch for readability.
- **Letter spacing**: -0.02em for headings, 0 for body, 0.05em for captions.

### Color
- Background: `#f5f5f7`
- Surface: `#ffffff`
- Text primary: `#1d1d1f`
- Text secondary: `#6e6e73`
- Accent: `#0071e3`
- Destructive: `#ff3b30`
- Success: `#34c759`
- Border: `#d2d2d7`

### Spacing (8px grid)
xs 4, sm 8, md 16, lg 24, xl 32, xxl 48.

## 5. Components
- **Card**: white, 16px radius, subtle shadow, 24px padding.
- **Button primary**: blue fill, white text, 10px radius.
- **Button secondary**: transparent, blue text.
- **Input**: 10px radius, 1px border, focus ring blue.
- **Nav**: minimal text links.
- **Table**: subtle dividers, generous padding.

## 6. Motion
- 200ms ease transitions.
- Respect `prefers-reduced-motion`.

## 7. Layout
- Max width 980px, centered.
- Single column mobile; two-column desktop.

## 8. Do / Don't
- Do: generous whitespace, one CTA per screen, clear empty states.
- Don't: clutter, multiple competing colors, dense tables, jargon.
## Materialization
The tokens live in `assets/app.css` (served from the binary — no CDN). Light is
the default; `[data-theme="dark"]` mirrors it. Change the tokens there, and the
whole console follows. The operator console layout: left rail + ledger-style
tables; setup and sign-in use the focused front-door shell.
