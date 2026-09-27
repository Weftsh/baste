(function () {
  "use strict";

  // Copy buttons: data-copy names the element whose text to copy.
  function copyText(text) {
    if (navigator.clipboard && window.isSecureContext) {
      return navigator.clipboard.writeText(text);
    }
    return new Promise(function (resolve, reject) {
      var area = document.createElement("textarea");
      area.value = text;
      area.setAttribute("readonly", "");
      area.style.position = "fixed";
      area.style.opacity = "0";
      document.body.appendChild(area);
      area.select();
      var ok = false;
      try { ok = document.execCommand("copy"); } catch (e) { ok = false; }
      document.body.removeChild(area);
      if (ok) { resolve(); } else { reject(new Error("copy failed")); }
    });
  }

  document.querySelectorAll("[data-copy]").forEach(function (button) {
    var label = button.textContent;
    button.addEventListener("click", function () {
      var source = document.getElementById(button.getAttribute("data-copy"));
      if (!source) { return; }
      copyText(source.textContent.trim()).then(function () {
        button.textContent = "Copied";
        setTimeout(function () { button.textContent = label; }, 1600);
      }, function () {
        var range = document.createRange();
        range.selectNodeContents(source);
        var selection = window.getSelection();
        selection.removeAllRanges();
        selection.addRange(range);
      });
    });
  });

  // Mobile menu.
  var toggle = document.querySelector("[data-menu-toggle]");
  var menu = document.querySelector("[data-menu]");
  if (toggle && menu) {
    var setOpen = function (open) {
      menu.classList.toggle("hidden", !open);
      toggle.setAttribute("aria-expanded", String(open));
      toggle.setAttribute("aria-label", open ? "Close menu" : "Open menu");
    };
    toggle.addEventListener("click", function () {
      setOpen(toggle.getAttribute("aria-expanded") !== "true");
    });
    menu.addEventListener("click", function (event) {
      if (event.target.closest("a")) { setOpen(false); }
    });
  }

  // GitHub star count, shown once it's a number worth showing.
  var stars = document.querySelectorAll("[data-stars]");
  if (stars.length && window.fetch) {
    fetch("https://api.github.com/repos/weftsh/baste", { headers: { Accept: "application/vnd.github+json" } })
      .then(function (response) { return response.ok ? response.json() : null; })
      .then(function (repo) {
        var count = repo && repo.stargazers_count;
        if (typeof count !== "number" || count < 25) { return; }
        var text = count >= 1000 ? (count / 1000).toFixed(count >= 10000 ? 0 : 1).replace(/\.0$/, "") + "k" : String(count);
        stars.forEach(function (el) {
          el.textContent = text;
          el.classList.remove("hidden");
        });
      })
      .catch(function () {});
  }
})();
