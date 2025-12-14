import { redirect } from "@sveltejs/kit";
async function load({ fetch }) {
  const res = await fetch("/api/v1/setup/status");
  if (!res.ok) {
    return { setupMode: true };
  }
  const data = await res.json();
  if (data.setupMode) {
    throw redirect(302, "/setup");
  }
  throw redirect(302, "/app");
}
export {
  load
};
