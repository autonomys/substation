import * as http from 'http';
import { fetch } from 'undici';
import * as dotenv from "dotenv";

dotenv.config();

const data = {
  uniqueAddressCount: "",
  spacePledged: "",
};

async function fetchMetadata() {
  const requestOptions = {
    method: 'POST',
    headers: {
      'x-api-key': process.env.SUBSCAN_API_KEY as string,
      'Content-Type': 'application/json',
    },
  };

  try {
    const response = await fetch(
      'https://subspace.api.subscan.io/api/scan/metadata',
      requestOptions
    );
    const json = await response.json();

    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    return (json as any).data;
  } catch (error) {
    console.log(`Failed to fetch from Subscan: ${error}`);
  }
}

async function updateMetadata() {
  const metadata = await fetchMetadata();

  if (metadata) {
    data.uniqueAddressCount = metadata.qualifiedRewardAddressesCount;
    data.spacePledged = metadata.consensusSpace;
  }
}

(async () => {
  try {
    await updateMetadata();
    setInterval(async () => await updateMetadata(), 10000);

    const server = http.createServer(async (req, res) => {
      const headers = {
        'Access-Control-Allow-Origin': '*',
        'Access-Control-Allow-Methods': 'OPTIONS, POST, GET',
        'Access-Control-Max-Age': 2592000,
        'Content-Type': 'application/json',
      };

      if (req.method === 'OPTIONS') {
        res.writeHead(204, headers);
        res.end();
        return;
      }

      if (req.url === '/api') {
        res.writeHead(200, headers);
        res.end(JSON.stringify(data));
      } else {
        res.statusCode = 404;
        res.end('Not found');
      }
    });

    server.listen(8080);
  } catch (error) {
    console.log(error);
  }
})();

