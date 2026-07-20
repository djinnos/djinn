#!/usr/bin/env perl
# Deterministic wrapper for the ste6 fixture contract. The Rust example it calls
# uses the landed RepoGraphArtifact bincode serializer/reader and galaxy payload
# schema; this file owns only fixture orchestration and board projection rows.
use strict; use warnings;
use Digest::SHA qw(sha256_hex); use JSON::PP; use File::Path qw(remove_tree make_path);
use Getopt::Long qw(GetOptions);
my $HERE = __FILE__; $HERE =~ s{/[^/]+$}{}; my $ROOT = $HERE; $ROOT =~ s{/?server/docs/operational/memory-sustainability/fixtures$}{}; $ROOT='.' if $ROOT eq '';
my ($profile, $output, $validate, $validate_only, $help) = ('smoke', undef, 0, 0, 0);
GetOptions('profile=s'=>\$profile, 'output-dir=s'=>\$output, 'validate'=>\$validate, 'validate-only'=>\$validate_only, 'help'=>\$help) or die "invalid options\n";
die "usage: perl generate.pl --profile production|smoke --output-dir DIR [--validate|--validate-only]\n" if $help || !$output;
my $json = JSON::PP->new->canonical(1)->pretty(1);
open my $mf, '<:raw', "$HERE/manifest.json" or die "$!"; local $/; my $manifest = decode_json(<$mf>); close $mf;
die "unknown profile $profile\n" unless exists $manifest->{profiles}{$profile}; my $p = $manifest->{profiles}{$profile};
sub fail { die "memory-sustainability fixture validation failed: $_[0]\n" }
sub cjson { $json->encode($_[0]) }
sub write_raw { my ($path,$bytes)=@_; open my $fh,'>:raw',$path or die "$path: $!"; print $fh $bytes; close $fh }
sub read_raw { my ($path)=@_; open my $fh,'<:raw',$path or die "$path: $!"; local $/; my $x=<$fh>; close $fh; $x }
sub task_id { sprintf('00000000-0000-7000-8000-%012x', $_[0]) }
# Mirrors the landed BoardHealthMismatchCandidate role-signal eligibility for
# these fixture fields; no scanner or repository implementation is duplicated.
sub eligible { my ($t)=@_; my $text=lc join("\n", map {$t->{$_}//''} qw(title description design acceptance_criteria)); my $planner=$t->{issue_type}=~/^(planning|decomposition)$/ || $text =~ /task_create/; my $dispatched=$t->{issue_type}=~/^(planning|decomposition)$/ && $t->{status}!~/^(needs_task_review|in_task_review|needs_lead_intervention|in_lead_intervention)$/; $t->{total_reopen_count}>=3 && $t->{status} ne 'closed' && $planner && !$dispatched }
sub compare_expected { my ($p,$r)=@_; for my $k (keys %{$p->{checksums}//{}}){fail("checksum drift for $k") unless $r->{checksums}{$k} eq $p->{checksums}{$k}} }
sub graph_tool {
  my ($action,$p,$out)=@_; my $g=$p->{graph}; my $a=$p->{galaxy_artifact};
  my @cmd=('cargo','run','--quiet','--manifest-path',"$ROOT/server/Cargo.toml",'-p','djinn-graph','--example','memory_sustainability_fixture','--',$action,$out,$g->{requested_nodes},$g->{requested_edges},$g->{requested_blob_bytes},$a->{requested_chunks},$a->{requested_total_bytes});
  system @cmd; fail("landed graph/artifact $action failed (exit ".($?>>8).")") if $? != 0;
}
sub generate {
  my ($name,$p,$out)=@_; remove_tree($out); make_path($out); graph_tool('generate',$p,$out);
  my @rows; for my $i (1..$p->{board_health}{requested_eligible_tasks}) { push @rows, JSON::PP->new->canonical(1)->encode({id=>task_id($i),short_id=>sprintf('ste6-%05d',$i),epic_id=>undef,title=>"memory sustainability fixture $i",description=>'requires task_create',design=>'',acceptance_criteria=>'[]',issue_type=>'task',status=>'open',total_reopen_count=>3}) }
  my $tasks=join("\n",@rows)."\n"; write_raw("$out/board-health-tasks.jsonl",$tasks);
  my $graph=read_raw("$out/canonical-graph.blob"); my $artifact_bytes=read_raw("$out/galaxy-artifact/manifest.json"); my $artifact=decode_json($artifact_bytes);
  my $r={schema=>'memory-sustainability-fixture-report/v2',profile=>$name,seed=>$p->{seed},observed=>{graph_nodes=>$p->{graph}{requested_nodes},graph_edges=>$p->{graph}{requested_edges},graph_blob_bytes=>length($graph),board_eligible_tasks=>scalar(@rows),artifact_chunks=>$artifact->{chunk_count},artifact_bytes=>$artifact->{byte_count}},checksums=>{canonical_graph_sha256=>sha256_hex($graph),board_tasks_sha256=>sha256_hex($tasks),artifact_transport_sha256=>$artifact->{transport_sha256},artifact_manifest_sha256=>sha256_hex($artifact_bytes)}}; compare_expected($p,$r); write_raw("$out/fixture-report.json",cjson($r)); $r
}
sub validate_output {
  my ($name,$p,$out)=@_; for my $x ('canonical-graph.blob','board-health-tasks.jsonl','galaxy-artifact/manifest.json','fixture-report.json'){fail("missing $x") unless -f "$out/$x"}
  my $graph=read_raw("$out/canonical-graph.blob"); my $g=$p->{graph}; fail('canonical graph blob byte count drift') unless length($graph)==$g->{requested_blob_bytes}; if(my $range=$g->{required_blob_range_bytes}) { fail('canonical graph blob is outside required 65-70 MiB range') unless length($graph)>=$range->[0] && length($graph)<=$range->[1] }
  graph_tool('validate',$p,$out); # deserializes real bincode plus gzip payload schema/hash
  my @rows=map {decode_json($_)} grep {length} split(/\n/,read_raw("$out/board-health-tasks.jsonl")); fail('board eligibility/count drift') unless @rows==$p->{board_health}{requested_eligible_tasks} && !grep {!eligible($_)} @rows;
  my $a_bytes=read_raw("$out/galaxy-artifact/manifest.json"); my $a=decode_json($a_bytes); my $c=$p->{galaxy_artifact}; fail('artifact manifest metadata drift') unless $a->{schema} eq 'galaxy-artifact-spool-fixture/v2' && $a->{artifact_version}==1 && $a->{encoding} eq 'gzip' && $a->{generation_id} eq $a->{artifact_id} && $a->{graph_content_hash}=~/\A[0-9a-f]{64}\z/;
  opendir my $dh,"$out/galaxy-artifact" or fail("cannot inspect artifact directory: $!"); my @files=sort grep {!/^manifest\.json$/} readdir($dh); closedir $dh; my @wanted=map {sprintf('chunk-%05d.bin',$_)} 0..$c->{requested_chunks}-1; fail('artifact directory has unexpected, duplicate, nonconforming, missing, or out-of-range chunk files') unless @files==@wanted && join("\0",@files) eq join("\0",@wanted);
  fail('artifact count or total byte drift') unless $a->{chunk_count}==$c->{requested_chunks} && $a->{byte_count}==$c->{requested_total_bytes} && @{$a->{chunk_hashes}}==$a->{chunk_count}; my $transport=''; my $total=0; for my $i (0..$a->{chunk_count}-1){my $b=read_raw(sprintf("$out/galaxy-artifact/chunk-%05d.bin",$i)); fail("chunk $i contiguity, size, or checksum drift") unless length($b)==$c->{requested_chunk_bytes} && sha256_hex($b) eq $a->{chunk_hashes}[$i]; $total+=length($b); $transport.=$b} fail('artifact transport checksum or total byte drift') unless $total==$a->{byte_count} && sha256_hex($transport) eq $a->{transport_sha256};
  my $r=decode_json(read_raw("$out/fixture-report.json")); my %got=(canonical_graph_sha256=>sha256_hex($graph),board_tasks_sha256=>sha256_hex(read_raw("$out/board-health-tasks.jsonl")),artifact_transport_sha256=>$a->{transport_sha256},artifact_manifest_sha256=>sha256_hex($a_bytes)); for my $k(keys %got){fail("fixture report checksum drift for $k") unless $r->{checksums}{$k} eq $got{$k}} compare_expected($p,$r); $r
}
my $report=$validate_only ? validate_output($profile,$p,$output) : generate($profile,$p,$output); validate_output($profile,$p,$output) if $validate && !$validate_only; print cjson($report);
