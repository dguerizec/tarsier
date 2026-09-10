// Shared by the settings library and the preview pickers.
export function avatarDeleteButton(item, kind, onDeleted) {
  if (item.selected) return null;
  const button = document.createElement('button');
  button.type = 'button';
  button.className = 'avatar-delete';
  button.textContent = '×';
  button.setAttribute('aria-label', `Delete ${item.name}`);
  button.title = `Delete ${item.name}`;
  button.addEventListener('click', event => {
    event.preventDefault();
    event.stopPropagation();
    const dialog = document.createElement('dialog');
    dialog.innerHTML = '<form><h2>Delete avatar?</h2><p class="avatar-delete-name"></p><p>The avatar will be removed from the library. Original files are kept on disk.</p><p class="error" role="alert" hidden></p><div class="dialog-actions"><button type="button" class="secondary">Cancel</button><button type="submit" class="danger">Delete</button></div></form>';
    dialog.querySelector('.avatar-delete-name').textContent = item.name;
    const cancel = dialog.querySelector('[type="button"]');
    const confirm = dialog.querySelector('[type="submit"]');
    let pending = false;
    cancel.addEventListener('click', () => dialog.close());
    dialog.addEventListener('cancel', event => { if (pending) event.preventDefault(); });
    dialog.addEventListener('close', () => { dialog.remove(); if (button.isConnected) button.focus(); });
    dialog.querySelector('form').addEventListener('submit', async event => {
      event.preventDefault();
      if (pending) return;
      pending = true;
      cancel.disabled = confirm.disabled = true;
      const error = dialog.querySelector('.error');
      error.hidden = true;
      try {
        const response = await fetch(`/api/v1/settings/avatars/${kind}/${encodeURIComponent(item.id)}`, {method: 'DELETE'});
        if (!response.ok) {
          const result = await response.json().catch(() => ({}));
          throw new Error(result.error || 'Could not delete avatar');
        }
        await onDeleted();
        dialog.close();
      } catch (failure) {
        error.textContent = failure.message;
        error.hidden = false;
      } finally {
        pending = false;
        cancel.disabled = confirm.disabled = false;
      }
    });
    document.body.append(dialog);
    dialog.showModal();
    cancel.focus();
  });
  return button;
}
