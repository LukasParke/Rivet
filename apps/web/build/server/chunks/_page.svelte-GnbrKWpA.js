import { a as attr } from './attributes-BeaNKpgU.js';
import { p as push, c as pop } from './index-DyZnF-0d.js';
import './escaping-CqgfEcN3.js';

function _page($$payload, $$props) {
  push();
  let email = "";
  let password = "";
  let busy = false;
  $$payload.out += `<div class="card"><h2>Create Admin</h2> <p class="muted">Password must be at least 12 characters.</p> <div class="row"><input placeholder="Email"${attr("value", email)}> <input placeholder="Password" type="password"${attr("value", password)}> <button${attr("disabled", busy, true)}>Create</button></div> `;
  {
    $$payload.out += "<!--[!-->";
  }
  $$payload.out += `<!--]--></div>`;
  pop();
}

export { _page as default };
//# sourceMappingURL=_page.svelte-GnbrKWpA.js.map
