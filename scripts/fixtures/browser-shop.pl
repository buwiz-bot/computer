#!/usr/bin/perl
# A page for the image smoke's browser steps, served inside the box on 127.0.0.1:6910 by a
# command's user (above the box's port floor), so the steps need no internet. It has a field to
# type into, a password field whose value must never come back, a plain button, a button the
# confirm globs catch, a form whose submit button they catch too (an Enter in its field sends it),
# and a cookie and local storage for the login to save. /login is a fake sign-in form (an email
# field, a password field and a "Sign in" button the confirm globs don't catch) for the steps that
# fill a login through the routes Buwiz Bot's broker uses; it says who signed in, never the password.
use strict;
use warnings;
use IO::Select;
use IO::Socket::INET;

my $page = <<'HTML';
<!doctype html>
<html><head><title>Fixture shop</title></head><body>
<h1>Fixture shop</h1>
<label for="email">Email</label> <input id="email" type="email">
<label for="pw">Password</label> <input id="pw" type="password" value="hunter2-smoke-pw">
<button id="search" onclick="document.getElementById('out').textContent = 'searched ' + document.getElementById('email').value">Search</button>
<button id="pay" onclick="document.getElementById('out').textContent = 'paid in full'">Pay $18</button>
<form id="checkout" onsubmit="event.preventDefault(); document.getElementById('out').textContent = 'order placed'">
<label for="card">Card</label> <input id="card" type="text">
<button type="submit">Place order</button>
</form>
<p id="out">ready</p>
<script>localStorage.setItem('fixture', '1'); document.cookie = 'fixture=1; path=/';</script>
</body></html>
HTML

my $login = <<'HTML';
<!doctype html>
<html><head><title>Fixture sign-in</title></head><body>
<h1>Fixture sign-in</h1>
<form id="signin" onsubmit="event.preventDefault(); document.getElementById('who').textContent = (document.getElementById('pw').value ? 'signed in as ' : 'no password for ') + document.getElementById('email').value">
<label for="email">Email</label> <input id="email" type="email" autocomplete="username">
<label for="pw">Password</label> <input id="pw" type="password" autocomplete="current-password">
<button type="submit">Sign in</button>
</form>
<p id="who">signed out</p>
</body></html>
HTML

my $server = IO::Socket::INET->new(
    LocalAddr => '127.0.0.1',
    LocalPort => 6910,
    Listen    => 16,
    ReuseAddr => 1,
) or die "listen: $!";
# One connection at a time, so each gets 2 s to send its request head: a connection that sends
# nothing (a browser's preconnect) is dropped instead of holding up the next request.
sub head_of {
    my ($client) = @_;
    my ($head, $select) = ('', IO::Select->new($client));
    while ($head !~ /\r?\n\r?\n/) {
        return undef unless $select->can_read(2);
        my $got = sysread($client, my $chunk, 4096);
        return undef unless $got;
        $head .= $chunk;
    }
    return $head;
}
while (my $client = $server->accept) {
    my $head = head_of($client);
    if (!defined $head) {
        close $client;
        next;
    }
    my $body = $head =~ m{^GET /login[ ?]} ? $login : $page;
    print $client "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n"
      . "Content-Length: " . length($body) . "\r\nConnection: close\r\n\r\n" . $body;
    close $client;
}
