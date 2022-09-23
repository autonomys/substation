import * as http from 'http';
import { fetch } from 'undici';
import * as dotenv from "dotenv";

dotenv.config();

const data = {
  uniqueAddressCount: 0,
};

async function fetchAddresses() {
  const body = JSON.stringify({
    filter: '',
    row: 1,
    page: 0,
    order: 'desc',
    order_field: 'balance',
  });

  const requestOptions = {
    method: 'POST',
    headers: {
      'x-api-key': process.env.SUBSCAN_API_KEY as string,
      'Content-Type': 'application/json',
    },
    body,
  };

  try {
    const response = await fetch(
      'https://subspace.api.subscan.io/api/scan/accounts',
      requestOptions
    );
    const json = await response.json();

    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    return (json as any).data;
  } catch (error) {
    console.log(`Failed to fetch from Subscan: ${error}`);
  }
}

async function updateAddressCount() {
  const addresses = await fetchAddresses();

  if (addresses) {
    console.log('addresses.count', addresses.count);
    // remove vesting accounts
    data.uniqueAddressCount = addresses.count - 18;
  }
}

(async () => {
  try {
    await updateAddressCount();
    setInterval(async () => await updateAddressCount(), 10000);

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

    server.listen(8000);
  } catch (error) {
    console.log(error);
  }
})();

