(() => {
  "use strict";

  const form = document.getElementById("login-form");
  const username = document.getElementById("username");
  const password = document.getElementById("password");
  const error = document.getElementById("login-error");
  const submit = document.getElementById("login-submit");

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    error.hidden = true;
    submit.disabled = true;
    try {
      const response = await fetch("login", {
        method: "POST",
        credentials: "same-origin",
        headers: {
          "Content-Type": "application/json",
          "X-MMUX": "1",
        },
        body: JSON.stringify({
          username: username.value,
          password: password.value,
        }),
      });
      let data = null;
      try {
        data = await response.json();
      } catch {
        data = null;
      }
      if (response.ok) {
        window.location.replace("./");
        return;
      }
      if (response.status === 429) {
        error.textContent = "Too many attempts. Try again in a few seconds.";
      } else if (data && data.error) {
        error.textContent = data.error;
      } else {
        error.textContent = "Sign in failed.";
      }
      error.hidden = false;
      password.value = "";
      password.focus();
    } catch {
      error.textContent = "Could not reach mmux.";
      error.hidden = false;
    } finally {
      submit.disabled = false;
    }
  });
})();
