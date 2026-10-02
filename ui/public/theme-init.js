// Runs before first paint (external file: the gateway's CSP forbids inline scripts).
(function () {
  var pref = null;
  try { pref = localStorage.getItem('switchyard.theme'); } catch (e) {}
  var dark = pref ? pref === 'dark' : !window.matchMedia || window.matchMedia('(prefers-color-scheme: dark)').matches;
  var t = dark ? 'dark' : 'light';
  document.documentElement.setAttribute('data-theme', t);
  document.documentElement.style.colorScheme = t;
})();
