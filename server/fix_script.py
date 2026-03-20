import sys

with open('src/sync/tasks_channel.rs', 'r') as f:
    content = f.read()

old = 'match TaskRepository::reconcile_peer_in_tx(&mut tx, peer_user_id, task_ids).await {'
new = 'match TaskRepository::reconcile_peer_in_tx(&mut tx, peer_user_id, &task_ids.iter().cloned().collect::<Vec<_>>()).await {'

if old in content:
    content = content.replace(old, new)
    with open('src/sync/tasks_channel.rs', 'w') as f:
        f.write(content)
    print('Fixed!')
    sys.exit(0)
else:
    print('Pattern not found, checking line 363...')
    lines = content.split('\n')
    if len(lines) > 362:
        print(f'Line 363: {lines[362]}')
    sys.exit(1)
