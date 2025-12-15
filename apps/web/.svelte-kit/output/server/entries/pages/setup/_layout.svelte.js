import { d as slot } from "../../../chunks/index.js";
function _layout($$payload, $$props) {
  $$payload.out += `<div class="container"><h1>Setup</h1> <p class="muted">First-run guided setup wizard.</p> <!---->`;
  slot($$payload, $$props, "default", {});
  $$payload.out += `<!----></div>`;
}
export {
  _layout as default
};
