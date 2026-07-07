<?php
declare(strict_types=1);

/*
 * Dynamic key API example for DV-HLS Gateway.
 *
 * Start:
 *   DVHLS_KEY_API_TOKEN='change-this-token' \
 *   DVHLS_KEY_STORE='./keys.json' \
 *   php -S 127.0.0.1:45689 examples/key_api.php
 *
 * keys.json format:
 * {
 *   "00112233445566778899aabbccddeeff": "ffeeddccbbaa99887766554433221100"
 * }
 */

function respond(int $status, mixed $body): never
{
    http_response_code($status);
    header('content-type: application/json; charset=utf-8');
    echo json_encode($body, JSON_UNESCAPED_SLASHES | JSON_UNESCAPED_UNICODE) . "\n";
    exit;
}

function normalize_hex_id(string $value): ?string
{
    $value = strtolower(trim($value));
    if (str_starts_with($value, '0x')) {
        $value = substr($value, 2);
    }
    $value = str_replace('-', '', $value);
    return preg_match('/^[0-9a-f]{32}$/', $value) === 1 ? $value : null;
}

if ($_SERVER['REQUEST_METHOD'] !== 'POST') {
    respond(405, ['error' => 'method not allowed']);
}

$expectedToken = getenv('DVHLS_KEY_API_TOKEN') ?: '';
if ($expectedToken === '') {
    respond(500, ['error' => 'DVHLS_KEY_API_TOKEN is not configured']);
}

$providedToken = $_SERVER['HTTP_X_TOKEN'] ?? '';
if (!hash_equals($expectedToken, $providedToken)) {
    respond(401, ['error' => 'unauthorized']);
}

$raw = file_get_contents('php://input');
$request = json_decode($raw === false ? '' : $raw, true);
if (!is_array($request) || !isset($request['kid']) || !is_array($request['kid'])) {
    respond(400, ['error' => 'request body must be {"kid":["..."]}']);
}

$storePath = getenv('DVHLS_KEY_STORE') ?: './keys.json';
$storeText = is_file($storePath) ? file_get_contents($storePath) : false;
if ($storeText === false) {
    respond(500, ['error' => 'key store not found']);
}

$storeJson = json_decode($storeText, true);
if (!is_array($storeJson)) {
    respond(500, ['error' => 'key store must be a JSON object']);
}

$store = [];
foreach ($storeJson as $kid => $key) {
    if (!is_string($kid) || !is_string($key)) {
        continue;
    }
    $normalizedKid = normalize_hex_id($kid);
    $normalizedKey = normalize_hex_id($key);
    if ($normalizedKid !== null && $normalizedKey !== null) {
        $store[$normalizedKid] = $normalizedKey;
    }
}

$result = [];
$missing = [];
foreach ($request['kid'] as $kid) {
    if (!is_string($kid)) {
        respond(400, ['error' => 'kid items must be strings']);
    }
    $normalizedKid = normalize_hex_id($kid);
    if ($normalizedKid === null) {
        respond(400, ['error' => 'invalid kid', 'kid' => $kid]);
    }
    if (!array_key_exists($normalizedKid, $store)) {
        $missing[] = $normalizedKid;
        continue;
    }
    $result[] = $normalizedKid . ':' . $store[$normalizedKid];
}

if ($missing !== []) {
    respond(404, ['error' => 'missing key', 'missing' => array_values(array_unique($missing))]);
}

respond(200, $result);
