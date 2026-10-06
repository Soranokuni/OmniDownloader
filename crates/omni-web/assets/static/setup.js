/* First-run configuration (plan P2.4, P2.5). */

import { api, toast } from '/static/app.js?v=4';

const errorLine = document.getElementById('error');
const submitButton = document.getElementById('submit-btn');

// An existing administrator may reopen this page to reconfigure; in that case
// there is no account to create, so the section is hidden and its fields are
// not required.
const byId = (id) => document.getElementById(id);

api('/api/setup/state')
  .then((state) => {
    if (!state.needs_admin) {
      const section = byId('admin-section');
      section.hidden = true;
      for (const field of section.querySelectorAll('input')) field.required = false;
    }
    // A reopened page shows what is configured, so saving does not blank it.
    if (state.graph) {
      byId('graph-tenant').value = state.graph.tenant_id || '';
      byId('graph-client').value = state.graph.client_id || '';
      byId('graph-mailbox').value = state.graph.mailbox || '';
      if (state.graph.secret_set) byId('graph-secret').placeholder = '•••••••• (set; blank keeps it)';
    }
    if (state.watchfolder_path) byId('watchfolder').value = state.watchfolder_path;
    if (state.ollama_endpoint) byId('llm-endpoint').value = state.ollama_endpoint;
    if (state.ollama_model) byId('llm-model').value = state.ollama_model;
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
        graph_tenant_id: byId('graph-tenant').value.trim(),
        graph_client_id: byId('graph-client').value.trim(),
        graph_mailbox: byId('graph-mailbox').value.trim(),
        graph_client_secret: byId('graph-secret').value,
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
