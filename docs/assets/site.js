// jqf docs site chrome: the mobile nav toggle. The rail markup is static on
// every page; this is the only script both pages share.
(function () {
  var toggle = document.querySelector('.menu-toggle');
  var backdrop = document.querySelector('.nav-backdrop');
  var rail = document.querySelector('.rail');
  if (!toggle) return;

  function setOpen(open) {
    document.body.classList.toggle('nav-open', open);
    toggle.setAttribute('aria-expanded', String(open));
    toggle.setAttribute('aria-label', open ? 'Close menu' : 'Open menu');
  }

  function close() { setOpen(false); }

  toggle.addEventListener('click', function () {
    setOpen(!document.body.classList.contains('nav-open'));
  });
  if (backdrop) backdrop.addEventListener('click', close);
  if (rail) {
    rail.addEventListener('click', function (event) {
      if (event.target.closest('a')) close();
    });
  }
  document.addEventListener('keydown', function (e) {
    if (e.key === 'Escape' && document.body.classList.contains('nav-open')) {
      close();
      toggle.focus();
    }
  });
})();
