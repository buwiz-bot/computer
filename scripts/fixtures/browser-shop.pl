#!/usr/bin/perl
# A page for the image smoke's browser steps, served inside the box on 127.0.0.1:6910 by a
# command's user (above the box's port floor), so the steps need no internet. It has a field to
# type into, a password field whose value must never come back, a plain button, a button the
# confirm globs catch, and a cookie and local storage for the login to save.
use strict;
use warnings;
use IO::Socket::INET;

my $page = <<'HTML';
<!doctype html>
<html><head><title>Fixture shop</title></head><body>
<h1>Fixture shop</h1>
<label for="email">Email</label> <input id="email" type="email">
<label for="pw">Password</label> <input id="pw" type="password" value="hunter2-smoke-pw">
<button id="search" onclick="document.getElementById('out').textContent = 'searched ' + document.getElementById('email').value">Search</button>
<button id="pay" onclick="document.getElementById('out').textContent = 'paid in full'">Pay $18</button>
<p id="out">ready</p>
<script>localStorage.setItem('fixture', '1'); document.cookie = 'fixture=1; path=/';</script>
</body></html>
HTML

my $server = IO::Socket::INET->new(
    LocalAddr => '127.0.0.1',
    LocalPort => 6910,
    Listen    => 16,
    ReuseAddr => 1,
) or die "listen: $!";
while (my $client = $server->accept) {
    while (my $line = <$client>) {
        last if $line =~ /^\r?\n$/;
    }
    print $client "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n"
      . "Content-Length: " . length($page) . "\r\nConnection: close\r\n\r\n" . $page;
    close $client;
}
