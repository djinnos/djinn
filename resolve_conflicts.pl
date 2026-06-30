#!/usr/bin/perl
use strict;
use warnings;

my $file = $ARGV[0];
open my $fh, '<', $file or die "Cannot open $file: $!";
my $content = do { local $/; <$fh> };
close $fh;

# Keep both sides of each conflict
my $marker_head = '<' x 7 . ' HEAD';
my $marker_sep = '=' x 7;
my $marker_end = '>' x 7 . ' origin/main';

while ($content =~ /\Q$marker_head\E\n(.*?)\n\Q$marker_sep\E\n(.*?)\n\Q$marker_end\E\n/s) {
    my $head = $1;
    my $main = $2;
    my $replacement = $head . "\n" . $main . "\n";
    $content =~ s/\Q$marker_head\E\n.*?\n\Q$marker_sep\E\n.*?\n\Q$marker_end\E\n/$replacement/s;
}

open my $out, '>', $file or die "Cannot write $file: $!";
print $out $content;
close $out;
