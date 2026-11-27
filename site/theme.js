(() => {
  const root = document.documentElement;
  const storageKey = "hatchzip-theme";
  const systemTheme = window.matchMedia("(prefers-color-scheme: dark)");
  const isChinese = root.lang.toLowerCase().startsWith("zh");
  let savedTheme = null;

  try {
    const storedTheme = window.localStorage.getItem(storageKey);
    if (storedTheme === "light" || storedTheme === "dark") {
      savedTheme = storedTheme;
    }
  } catch (error) {
    console.error("Could not read the saved theme preference.", error);
  }

  const applyTheme = (theme) => {
    root.dataset.theme = theme;
  };

  applyTheme(savedTheme || (systemTheme.matches ? "dark" : "light"));

  const updateLabel = (button) => {
    const dark = root.dataset.theme === "dark";
    const label = isChinese
      ? dark ? "切换到浅色主题" : "切换到深色主题"
      : dark ? "Switch to light theme" : "Switch to dark theme";
    button.setAttribute("aria-label", label);
    button.title = label;
    button.dataset.icon = dark ? "sun" : "moon";
  };

  const addToggle = () => {
    const links = document.querySelector(".links");
    const container = links
      || document.querySelector(".side nav")
      || document.querySelector("header");
    if (!container) {
      return;
    }

    const button = document.createElement("button");
    button.className = "theme-toggle gh";
    button.type = "button";
    updateLabel(button);
    button.addEventListener("click", () => {
      const theme = root.dataset.theme === "dark" ? "light" : "dark";
      savedTheme = theme;
      applyTheme(theme);
      updateLabel(button);
      try {
        window.localStorage.setItem(storageKey, theme);
      } catch (error) {
        console.error("Could not save the theme preference.", error);
      }
    });
    if (links) {
      links.append(button);
    } else {
      container.prepend(button);
    }

    systemTheme.addEventListener("change", (event) => {
      if (savedTheme === null) {
        applyTheme(event.matches ? "dark" : "light");
        updateLabel(button);
      }
    });
  };

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", addToggle, { once: true });
  } else {
    addToggle();
  }
})();
