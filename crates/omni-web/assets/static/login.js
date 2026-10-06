/* Sign-in page (plan P2.2, P2.5). */

import { api, toast } from '/static/app.js?v=4';

const errorLine = document.getElementById('error');
const submitButton = document.getElementById('submit-btn');

// The first-run hint is a fact about the deployment, not about any account, so
// it is safe to show before anyone signs in -- and without it a fresh install
// looks broken: there is no seeded administrator to sign in as any more.
api('/api/setup/state')
  .then((state) => {
    document.getElementById('setup-hint').hidden = !state.needs_admin;
  })
  .catch(() => { /* not fatal; the hint simply stays hidden */ });

document.getElementById('login-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  errorLine.textContent = '';
  submitButton.disabled = true;
  submitButton.textContent = 'Σύνδεση…';

  try {
    const result = await api('/api/auth/login', {
      method: 'POST',
      body: {
        email: document.getElementById('email').value.trim(),
        password: document.getElementById('password').value,
      },
    });
    // Land on the panel the role actually uses.
    location.href =
      result.role === 'admin' ? '/admin' : result.role === 'open_mcr' ? '/mcr' : '/user';
  } catch (e) {
    if (e.code === 'RATE_LIMITED') {
      errorLine.textContent =
        'Πάρα πολλές αποτυχημένες προσπάθειες από αυτόν τον υπολογιστή. Περιμένετε λίγα λεπτά και ξαναδοκιμάστε.';
    } else {
      // One message for a wrong password and for an unknown address: the login
      // form must not reveal which newsroom addresses have accounts.
      errorLine.textContent = e.message;
    }
    toast(errorLine.textContent, 'bad');
    document.getElementById('password').value = '';
  } finally {
    submitButton.disabled = false;
    submitButton.textContent = 'Σύνδεση';
  }
});
