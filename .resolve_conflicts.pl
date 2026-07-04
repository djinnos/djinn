#!/usr/bin/perl -i -0777 -p
# Resolve git conflict markers by keeping HEAD (ours) side
s/<<<<<<< HEAD\n(.*?)\n=======\n.*?\n>>>>>>> origin-main\n/$1\n/gs;
