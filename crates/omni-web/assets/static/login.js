/* Sign-in page (plan P2.2, P2.5). */

import { api, toast } from '/static/app.js?v=6';

const errorLine = document.getElementById('error');
const submitButton = document.getElementById('submit-btn');

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
    // The first-run account lands on /admin, which shows only the change form.
    location.href = result.password_change_required ? '/admin'
      : result.role === 'admin' ? '/admin' : result.role === 'open_mcr' ? '/mcr' : '/user';
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
