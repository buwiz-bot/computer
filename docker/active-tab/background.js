// Tells the box the URL of the tab in front of the window last in front, every time it may have
// changed. The server reads it (box-host `GET /v1/chrome/active-tab`) before it types a saved
// login, and types nothing when the page in front is not the login's own site: on a computer
// several Bots share, another Bot may have brought its own page forward.
//
// With it, what kind of box has focus on that page (`focus.js`: password, text, other or none),
// so a password is typed only into a password box. Only the `tabs` permission and that kind:
// nothing of the page, its cookies or its storage. An open native port also keeps this service
// worker alive.
const HOST = "app.buwiz.active_tab";
let port = null;
// Each tab's focused box, as its page last said; a tab that has not said is unknown.
const focusByTab = new Map();

function connect() {
  port = chrome.runtime.connectNative(HOST);
  port.onDisconnect.addListener(() => {
    port = null;
    setTimeout(connect, 1000);
  });
  report();
}

async function report() {
  if (!port) return;
  const [tab] = await chrome.tabs.query({ active: true, lastFocusedWindow: true });
  try {
    const focus = tab && focusByTab.has(tab.id) ? focusByTab.get(tab.id) : null;
    port.postMessage({ url: tab ? tab.url || tab.pendingUrl || "" : "", focus });
  } catch (_) {
    // The port closed between the check and the send; onDisconnect reconnects.
  }
}

chrome.tabs.onActivated.addListener(report);
chrome.tabs.onUpdated.addListener((_id, change, tab) => {
  // A new page has said nothing of its focus yet.
  if (change.url) focusByTab.delete(_id);
  if (tab.active && (change.url || change.status)) report();
});
chrome.tabs.onRemoved.addListener((id) => {
  focusByTab.delete(id);
  report();
});
chrome.runtime.onMessage.addListener((message, sender) => {
  // Only a page's own top frame speaks for its focus.
  if (!sender.tab || sender.frameId !== 0 || typeof message.focus !== "string") return;
  focusByTab.set(sender.tab.id, message.focus);
  if (sender.tab.active) report();
});
chrome.windows.onFocusChanged.addListener(report);
connect();
