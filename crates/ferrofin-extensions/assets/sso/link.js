'use strict';
// The Single Sign-On link page's script: finds the session jellyfin-web keeps
// for this server in this browser, lists its account's linked sign-ins with
// it, and starts a link at a provider, with the account's password, or
// removes one. Its paths are relative (the page is `/sso/link`), so a server
// under a base URL keeps working. Every string from a response is set as
// text, never parsed as HTML. The password is sent once and never stored.
(async () => {
    const status = document.getElementById('status');
    const section = document.getElementById('links');
    const show = (message) => {
        status.textContent = message;
        status.hidden = !message;
    };
    const SIGN_IN = 'Sign in to Ferrofin in this browser first, then open this page again.';
    const ENDED = 'Your session has ended. Sign in to Ferrofin again, then open this page again.';
    try {
        // jellyfin-web's servers and tokens (jellyfin-apiclient's
        // `Credentials`, JSON under `jellyfin_credentials`, which the
        // completion page writes too), this server's found by its id.
        const info = await (await fetch('../System/Info/Public')).json();
        const credentials = JSON.parse(localStorage.getItem('jellyfin_credentials') || '{}');
        const server = (credentials.Servers || []).find((known) => known.Id === info.Id && known.AccessToken);
        if (!server) {
            show(SIGN_IN);
            return;
        }
        // The token authenticates each request: the server knows its device.
        const call = (method, path, body) => fetch(path, {
            method,
            credentials: 'same-origin',
            headers: Object.assign(
                { Authorization: 'MediaBrowser Token="' + encodeURIComponent(server.AccessToken) + '"' },
                body ? { 'Content-Type': 'application/json' } : {}
            ),
            body: body ? JSON.stringify(body) : undefined
        });
        const failed = (response, what) => {
            show(response.status === 401 ? ENDED : what + ' (HTTP ' + response.status + '). Try again.');
        };

        const passwordField = document.getElementById('password');
        const link = async (provider) => {
            const password = passwordField.value;
            if (!password) {
                show('Enter the password you sign in to Ferrofin with first.');
                passwordField.focus();
                return;
            }
            // Emptied before it is sent, so a failed request leaves it nowhere.
            passwordField.value = '';
            show('Opening ' + provider.Name + '…');
            const response = await call('POST', 'oidc/' + encodeURIComponent(provider.Key) + '/link',
                { Password: password });
            if (response.status === 403) {
                show('That password is not right. Try again.');
                passwordField.focus();
                return;
            }
            if (response.status === 409) {
                show('Your account is already linked at ' + provider.Name + ': remove that link first.');
                return;
            }
            if (response.status === 503) {
                show('Linking is not set up on this server yet: ask an administrator to set its public URL.');
                return;
            }
            if (response.status === 502) {
                show('The identity provider could not be reached. Try again later.');
                return;
            }
            if (response.status === 429) {
                show('Too many sign-in attempts came from your network. Wait a minute, then try again.');
                return;
            }
            if (!response.ok) {
                failed(response, 'Linking could not start');
                return;
            }
            // The flow cookie rode back on the answer: the provider sends the
            // browser back to the callback, which links and says so.
            location.assign((await response.json()).Url);
        };

        const remove = async (linked, last, ssoOnly) => {
            let question = 'Remove the link to ' + (linked.ProviderName || linked.Issuer)
                + '? Every session of your account is signed out, this one included.';
            if (last && ssoOnly) {
                question += '\n\n' + document.getElementById('warning').textContent;
            }
            if (!window.confirm(question)) {
                return;
            }
            const response = await call('DELETE', 'links/' + encodeURIComponent(linked.Provider));
            if (!response.ok) {
                failed(response, 'The link could not be removed');
                return;
            }
            // The server signed this session out: forget its token, so
            // jellyfin-web opens on its login page.
            const saved = JSON.parse(localStorage.getItem('jellyfin_credentials') || '{}');
            for (const known of saved.Servers || []) {
                if (known.Id === info.Id) {
                    delete known.AccessToken;
                    delete known.UserId;
                }
            }
            localStorage.setItem('jellyfin_credentials', JSON.stringify(saved));
            section.hidden = true;
            show('The link was removed, and your sessions were signed out. Sign in to Ferrofin again.');
        };

        const response = await call('GET', 'links');
        if (!response.ok) {
            failed(response, 'Your linked sign-ins could not be read');
            return;
        }
        const links = await response.json();
        const button = (label, onClick) => {
            const node = document.createElement('button');
            node.type = 'button';
            node.textContent = label;
            node.addEventListener('click', () => {
                onClick().catch(() => show('Something went wrong. Try again.'));
            });
            return node;
        };
        const linked = document.getElementById('linked');
        for (const entry of links.Links) {
            const item = document.createElement('li');
            item.textContent = (entry.ProviderName || entry.Issuer) + ', linked on '
                + new Date(entry.DateCreated).toLocaleDateString() + ' ';
            if (entry.Provider) {
                item.append(button('Remove', () => remove(entry, links.Links.length === 1,
                    links.SingleSignOnAccount)));
            } else {
                item.append('(its provider was removed: an administrator can remove this link)');
            }
            linked.append(item);
        }
        document.getElementById('none').hidden = links.Links.length > 0;
        document.getElementById('warning').hidden = !links.SingleSignOnAccount;
        // An account that signs in through Single Sign-On only has no
        // password to give: an administrator links it.
        document.getElementById('sso-only').hidden = !links.SingleSignOnAccount;
        document.getElementById('linking').hidden = links.SingleSignOnAccount;
        const providers = document.getElementById('providers');
        for (const provider of links.Providers) {
            const item = document.createElement('li');
            if (provider.Linked) {
                item.textContent = provider.Name + ': linked. Remove that link above before linking '
                    + 'another sign-in there.';
            } else {
                item.append(button('Link with ' + provider.Name, () => link(provider)));
            }
            providers.append(item);
        }
        document.getElementById('unavailable').hidden = links.Providers.length > 0;
        // The password is asked for only when a provider can be linked.
        document.getElementById('password-entry').hidden = !links.Providers.some((provider) => !provider.Linked);
        section.hidden = false;
        show('');
    } catch {
        show('Something went wrong. Try again.');
    }
})();
