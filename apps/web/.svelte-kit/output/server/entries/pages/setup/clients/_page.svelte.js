import { a as attr } from "../../../../chunks/attributes.js";
import { c as pop, p as push } from "../../../../chunks/index.js";
function _page($$payload, $$props) {
  push();
  let qbUrl = "http://qbittorrent:8080";
  let qbUser = "";
  let qbPass = "";
  let nzbUrl = "http://nzbget:6789";
  let nzbUser = "";
  let nzbPass = "";
  let sabUrl = "http://sabnzbd:8080";
  let sabKey = "";
  let includeQb = true;
  let includeNzb = true;
  let includeSab = false;
  let busy = false;
  $$payload.out += `<div class="card"><h2>Download clients</h2> <p class="muted">Configure at least one torrent and/or usenet client. Credentials are stored encrypted at rest.</p> <h3>qBittorrent</h3> <label><input type="checkbox"${attr("checked", includeQb, true)}> Enable</label> <div class="row" style="margin-top: 8px;"><input placeholder="URL"${attr("value", qbUrl)}${attr("disabled", !includeQb, true)}> <input placeholder="Username"${attr("value", qbUser)}${attr("disabled", !includeQb, true)}> <input placeholder="Password" type="password"${attr("value", qbPass)}${attr("disabled", !includeQb, true)}></div> <h3 style="margin-top: 16px;">NZBGet</h3> <label><input type="checkbox"${attr("checked", includeNzb, true)}> Enable</label> <div class="row" style="margin-top: 8px;"><input placeholder="URL"${attr("value", nzbUrl)}${attr("disabled", !includeNzb, true)}> <input placeholder="Username"${attr("value", nzbUser)}${attr("disabled", !includeNzb, true)}> <input placeholder="Password" type="password"${attr("value", nzbPass)}${attr("disabled", !includeNzb, true)}></div> <h3 style="margin-top: 16px;">SABnzbd</h3> <label><input type="checkbox"${attr("checked", includeSab, true)}> Enable</label> <div class="row" style="margin-top: 8px;"><input placeholder="URL"${attr("value", sabUrl)}${attr("disabled", !includeSab, true)}> <input placeholder="API key" type="password"${attr("value", sabKey)}${attr("disabled", !includeSab, true)}></div> <div class="row" style="margin-top: 12px;"><button${attr("disabled", busy, true)}>Validate</button> <button class="secondary"${attr("disabled", true, true)}>Save</button></div> `;
  {
    $$payload.out += "<!--[!-->";
  }
  $$payload.out += `<!--]--> `;
  {
    $$payload.out += "<!--[!-->";
  }
  $$payload.out += `<!--]--></div>`;
  pop();
}
export {
  _page as default
};
