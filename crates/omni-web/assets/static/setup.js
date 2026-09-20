/* First-run configuration (plan P2.4, P2.5). */

import { api, toast } from '/static/app.js?v=2';

const errorLine = document.getElementById('error');
const submitButton = document.getElementById('submit-btn');

// An existing administrator may reopen this page to reconfigure; in that case
// there is no account to create, so the section is hidden and its fields are
// not required.
api('/api/setup/state')
  .then((state) => {
    if (!state.needs_admin) {
      const section = document.getElementById('admin-section');
      section.hidden = true;
      for (const field of section.querySelectorAll('input')) field.required = false;
    }
  })
  .catch(() => { /* leave the section visible; the server decides regardless */ });

document.getElementById('setup-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  errorLine.textContent = '';
  submitButton.disabled = true;
  submitButton.textContent = 'Saving…';

  const adminVisible = !document.getElementById('admin-section').hidden;

  try {
    await api('/api/setup', {
      method: 'POST',
      body: {
        email_provider: document.getElementById('email-provider').value,
        imap_server: document.getElementById('imap-server').value.trim(),
        email_address: document.getElementById('email-address').value.trim(),
        email_password: document.getElementById('email-password').value,
        ollama_endpoint: document.getElementById('llm-endpoint').value.trim(),
        ollama_model: document.getElementById('llm-model').value.trim(),
        watchfolder_path: document.getElementById('watchfolder').value.trim(),
        admin_email: adminVisible ? document.getElementById('admin-email').value.trim() : null,
        admin_password: adminVisible ? document.getElementById('admin-password').value : null,
        admin_full_name: adminVisible ? document.getElementById('admin-name').value.trim() : null,
      },
    });
    toast('Setup complete. Sign in with the account you just created.', 'ok');
    setTimeout(() => { location.href = '/login'; }, 1400);
  } catch (e) {
    errorLine.textContent = e.message;
    toast(e.message, 'bad');
  } finally {
    submitButton.disabled = false;
    submitButton.textContent = 'Finish setup';
  }
});
