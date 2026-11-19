# GEMINI.md — DroidPath (Gemini CLI & Antigravity)

<!-- DROIDPATH_MCP_RULES_START -->
# DroidPath Android Control Protocol (Shadow DOM & Layer 3 A* Router)

Follow this strict protocol whenever controlling an Android device through DroidPath MCP tools:

1. **Navigate First with `auto_explore: true`**:
   - For any multi-step goal or known screen destination, ALWAYS call `android_navigate_to_node(target: "...", auto_explore: true)` first.
   - It computes the shortest A* route across the persistent Spatial Graph (`shadow_graph.json`) and automatically falls back to live Shadow DOM discovery (`android_explore_and_click`) if no route exists yet.
2. **Direct Discovery via `android_explore_and_click`**:
   - To find and tap a single element on the current screen (or via a breadcrumb prefix like `"Display > Night Light"`), call `android_explore_and_click(target: "...")`.
   - It scores live Shadow DOM nodes, auto-scrolls scrollable containers (up to `max_scrolls`, reversing at the bottom), dismisses occluding heads-up notifications, and records new edges into the Spatial Graph automatically.
3. **Zero Screenshots While Shadow DOM Is Alive**:
   - **NEVER** call `android_screenshot` (or ask for visual screenshots) while the Shadow DOM daemon stream is alive (`android_get_ui_tree`, `android_explore_and_click`, and `android_navigate_to_node` return sub-millisecond accessibility trees).
   - Use `android_get_ui_tree` only when you need to inspect structured labels/coordinates directly.
4. **Handle `needs_selection` Without Guessing**:
   - If `android_explore_and_click` (or `auto_explore: true`) returns `status: "needs_selection"` with a compressed top-20 PII-scrubbed `candidates` list, pick the exact matching candidate from that list and tap it or refine `target`. Do NOT blindly retry the same missing label.
5. **Mandatory Human-in-the-Loop (`requires_confirmation`)**:
   - If ANY tool returns `status: "requires_confirmation"` (`is_hazardous: true`, e.g., payment, purchase, transfer, send, delete, factory reset):
     - **STOP IMMEDIATELY.**
     - Show the user the exact `blocked_action` and `hazard_reason`.
     - Ask for explicit confirmation in chat.
     - **NEVER** pass `confirmed: true` without explicit user approval in the current conversation.
6. **Mandatory Biometric / `FLAG_SECURE` Privacy Pause (`secure_screen_active`)**:
   - If a tool returns `status: "secure_screen_active"` (`code: "SECURE_SCREEN_ACTIVE"`):
     - **STOP IMMEDIATELY.** Do not attempt taps, swipes, or screenshots.
     - Ask the user to unlock/authenticate on the physical Android device and tell you when ready.
   - Never ask for, echo, or persist raw passwords, payment card PANs, CVV/CVC, or OTP codes. DroidPath redacts password fields to `***` on-device and scrubs PANs (`<CARD>`) and OTP codes (`<OTP>`) before hashing or persistence.
7. **Minimal-Token Efficiency (Cloud & Local / On-Device Models)**:
   - Prefer 1 high-level `android_navigate_to_node(target, auto_explore: true)` or `android_explore_and_click(target)` call over multi-turn `android_get_ui_tree` + coordinate tap loops.
<!-- DROIDPATH_MCP_RULES_END -->
