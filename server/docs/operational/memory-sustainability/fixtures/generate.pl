#!/usr/bin/env perl
# Deterministic, stdlib-plus-core Perl generator for the ste6 fixture contract.
# It does not write database rows or duplicate production repositories/scanners:
# board rows are BoardHealthMismatchCandidate projection rows and artifact rows
# mirror GalaxyArtifactSpool/ReservedGalaxyArtifactChunk ordering/hash fields.
use strict; use warnings;
use Digest::SHA qw(sha256 sha256_hex); use JSON::PP; use File::Path qw(remove_tree make_path);
use File::Spec; use Getopt::Long qw(GetOptions);
my $HERE = __FILE__; $HERE =~ s{/[^/]+$}{};
my ($profile, $output, $validate, $validate_only, $help) = ('smoke', undef, 0, 0, 0);
GetOptions('profile=s'=>\$profile, 'output-dir=s'=>\$output, 'validate'=>\$validate, 'validate-only'=>\$validate_only, 'help'=>\$help) or die "invalid options\n";
die "usage: perl generate.pl --profile production|smoke --output-dir DIR [--validate|--validate-only]\n" if $help || !$output;
my $json = JSON::PP->new->canonical(1)->pretty(1);
open my $mf, '<:raw', "$HERE/manifest.json" or die "$!"; local $/; my $manifest = decode_json(<$mf>); close $mf;
die "unknown profile $profile\n" unless exists $manifest->{profiles}{$profile}; my $p = $manifest->{profiles}{$profile};
sub fail { die "memory-sustainability fixture validation failed: $_[0]\n" }
sub cjson { return $json->encode($_[0]) }
sub cjson_line { return JSON::PP->new->canonical(1)->encode($_[0])."\n" }
sub write_raw { my ($path,$bytes)=@_; open my $fh,'>:raw',$path or die "$path: $!"; print $fh $bytes; close $fh }
sub read_raw { my ($path)=@_; open my $fh,'<:raw',$path or die "$path: $!"; local $/; my $x=<$fh>; close $fh; return $x }
sub words {
  my ($seed,$domain)=@_; my $state=unpack('V', substr(sha256("$seed\0$domain"),0,4)) || 0x6d2b79f5;
  return sub { $state ^= (($state << 13) & 0xffffffff); $state &= 0xffffffff; $state ^= ($state >> 17); $state &= 0xffffffff; $state ^= (($state << 5) & 0xffffffff); $state &= 0xffffffff; return $state };
}
sub bytes { my ($len,$seed,$domain)=@_; my $next=words($seed,$domain); my $out=''; while(length($out)<$len){$out .= pack('V',$next->())} return substr($out,0,$len) }
sub task_id { return sprintf('00000000-0000-7000-8000-%012x', $_[0]) }
sub graph_header { my ($name,$p)=@_; return cjson_line({schema=>'repo-graph-artifact-fixture/v1',persisted_by=>'canonical_graph bincode publication seam',profile=>$name,seed=>$p->{seed},version=>11,nodes=>$p->{graph}{requested_nodes},edges=>$p->{graph}{requested_edges},padding=>'deterministic opaque fixture bytes; production RepoGraphArtifact serialization remains owned by djinn-graph'}) }
sub eligible { my ($t)=@_; my $text=lc join("\n", map {$t->{$_}//''} qw(title description design acceptance_criteria)); my $planner=$t->{issue_type}=~/^(planning|decomposition)$/ || $text =~ /task_create/; my $dispatched=$t->{issue_type}=~/^(planning|decomposition)$/ && $t->{status}!~/^(needs_task_review|in_task_review|needs_lead_intervention|in_lead_intervention)$/; return $t->{total_reopen_count}>=3 && $t->{status} ne 'closed' && $planner && !$dispatched }
sub compare_expected { my ($p,$r)=@_; for my $k (keys %{$p->{checksums}//{}}){fail("checksum drift for $k") if $r->{checksums}{$k} ne $p->{checksums}{$k}} }
sub generate {
  my ($name,$p,$out)=@_; remove_tree($out); make_path("$out/galaxy-artifact");
  my $header=graph_header($name,$p); my $size=$p->{graph}{requested_blob_bytes}; fail('graph header exceeds requested blob bytes') if length($header)>=$size;
  my $graph=$header.bytes($size-length($header),$p->{seed},'canonical-graph-padding'); write_raw("$out/canonical-graph.blob",$graph);
  my @rows; for my $i (1..$p->{board_health}{requested_eligible_tasks}) { push @rows, JSON::PP->new->canonical(1)->encode({id=>task_id($i),short_id=>sprintf('ste6-%05d',$i),epic_id=>undef,title=>"memory sustainability fixture $i",description=>'requires task_create',design=>'',acceptance_criteria=>'[]',issue_type=>'task',status=>'open',total_reopen_count=>3}) }
  my $tasks=join("\n",@rows)."\n"; write_raw("$out/board-health-tasks.jsonl",$tasks);
  my (@chunks,$transport); $transport=Digest::SHA->new(256);
  for my $i (0..$p->{galaxy_artifact}{requested_chunks}-1) { my $b=bytes($p->{galaxy_artifact}{requested_chunk_bytes},$p->{seed},"galaxy-chunk-$i"); my $h=sha256_hex($b); write_raw(sprintf("$out/galaxy-artifact/chunk-%05d.bin",$i),$b); $transport->add($b); push @chunks,{bytes=>$b,sha256=>$h} }
  my $artifact={schema=>'galaxy-artifact-spool-fixture/v1',artifact_version=>1,encoding=>'gzip',profile=>$name,generation_id=>'018f7e8a-0000-7000-8000-000000000001',artifact_id=>'018f7e8a-0000-7000-8000-000000000001',chunk_count=>scalar(@chunks),byte_count=>0+@chunks*$p->{galaxy_artifact}{requested_chunk_bytes},chunk_hashes=>[map {$_->{sha256}} @chunks],transport_sha256=>$transport->hexdigest}; my $artifact_bytes=cjson($artifact); write_raw("$out/galaxy-artifact/manifest.json",$artifact_bytes);
  my $r={schema=>'memory-sustainability-fixture-report/v1',profile=>$name,seed=>$p->{seed},observed=>{graph_blob_bytes=>length($graph),board_eligible_tasks=>scalar(@rows),artifact_chunks=>scalar(@chunks),artifact_bytes=>$artifact->{byte_count}},checksums=>{canonical_graph_sha256=>sha256_hex($graph),board_tasks_sha256=>sha256_hex($tasks),artifact_transport_sha256=>$artifact->{transport_sha256},artifact_manifest_sha256=>sha256_hex($artifact_bytes)}}; compare_expected($p,$r); write_raw("$out/fixture-report.json",cjson($r)); return $r;
}
sub validate_output {
  my ($name,$p,$out)=@_; for my $x ('canonical-graph.blob','board-health-tasks.jsonl','galaxy-artifact/manifest.json','fixture-report.json'){fail("missing $x") unless -f "$out/$x"}
  my $graph=read_raw("$out/canonical-graph.blob"); my ($line)=split(/\n/,$graph,2); my $head=eval {decode_json($line)}; fail('canonical graph blob lacks JSON header') unless $head; fail('canonical graph header counts/schema drift') unless $head->{schema} eq 'repo-graph-artifact-fixture/v1' && $head->{nodes}==$p->{graph}{requested_nodes} && $head->{edges}==$p->{graph}{requested_edges}; fail('canonical graph blob byte count drift') unless length($graph)==$p->{graph}{requested_blob_bytes}; if(my $range=$p->{graph}{required_blob_range_bytes}) {fail('canonical graph blob is outside required 65-70 MiB range') unless length($graph)>=$range->[0] && length($graph)<=$range->[1]}
  my @rows=map {decode_json($_)} grep {length} split(/\n/,read_raw("$out/board-health-tasks.jsonl")); fail('board eligibility/count drift') unless @rows==$p->{board_health}{requested_eligible_tasks} && !grep {!eligible($_)} @rows;
  my $a=decode_json(read_raw("$out/galaxy-artifact/manifest.json")); my $c=$p->{galaxy_artifact}; fail('artifact count or total byte drift') unless $a->{chunk_count}==$c->{requested_chunks} && $a->{byte_count}==$c->{requested_total_bytes} && @{$a->{chunk_hashes}}==$a->{chunk_count}; my $tr=Digest::SHA->new(256); my $total=0; for my $i (0..$a->{chunk_count}-1){my $b=read_raw(sprintf("$out/galaxy-artifact/chunk-%05d.bin",$i)); fail("chunk $i contiguity, size, or checksum drift") unless length($b)==$c->{requested_chunk_bytes} && sha256_hex($b) eq $a->{chunk_hashes}[$i]; $total+=length($b); $tr->add($b)} fail('artifact transport checksum or total byte drift') unless $total==$a->{byte_count} && $tr->hexdigest eq $a->{transport_sha256}; my $r=decode_json(read_raw("$out/fixture-report.json")); my %got=(canonical_graph_sha256=>sha256_hex($graph),board_tasks_sha256=>sha256_hex(read_raw("$out/board-health-tasks.jsonl")),artifact_transport_sha256=>$a->{transport_sha256},artifact_manifest_sha256=>sha256_hex(cjson($a))); for my $k(keys %got){fail("fixture report checksum drift for $k") unless $r->{checksums}{$k} eq $got{$k}} compare_expected($p,$r); return $r;
}
my $report=$validate_only ? validate_output($profile,$p,$output) : generate($profile,$p,$output); validate_output($profile,$p,$output) if $validate && !$validate_only; print cjson($report);
